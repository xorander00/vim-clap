mod searcher;

use self::searcher::{SearchEngine, SearchingWorker};
use crate::find_usages::{CtagsSearcher, GtagsSearcher, QueryType, Usage, UsageMatcher, Usages};
use crate::paths::AbsPathBuf;
use crate::stdio_server::handler::CachedPreviewImpl;
use crate::stdio_server::job;
use crate::stdio_server::provider::{ClapProvider, Context};
use crate::tools::ctags::{get_language, TagsGenerator, CTAGS_EXISTS};
use crate::tools::gtags::GTAGS_EXISTS;
use anyhow::Result;
use filter::Query;
use futures::Future;
use itertools::Itertools;
use rayon::prelude::*;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::Instrument;

/// Internal reprentation of user input.
#[derive(Debug, Clone, Default)]
struct QueryInfo {
    /// Keyword for the tag or regex searching.
    keyword: String,
    /// Query type for `keyword`.
    query_type: QueryType,
    /// Search terms for further filtering.
    usage_matcher: UsageMatcher,
}

impl QueryInfo {
    /// Return `true` if the result of query info is a superset of the result of another,
    /// i.e., `self` contains all the search results of `other`.
    ///
    /// The rule is as follows:
    ///
    /// - the keyword is the same.
    /// - the new query is a subset of last query.
    fn is_superset(&self, other: &Self) -> bool {
        self.keyword == other.keyword
            && self.query_type == other.query_type
            && self.usage_matcher.is_superset(&other.usage_matcher)
    }
}

/// Parses the raw user input and returns the final keyword as well as the constraint terms.
/// Currently, only one keyword is supported.
///
/// `hel 'fn` => `keyword ++ exact_term/inverse_term`.
///
/// # Argument
///
/// - `query`: Initial query typed in the input window.
fn parse_query_info(query: &str) -> QueryInfo {
    let Query {
        exact_terms,
        inverse_terms,
        fuzzy_terms,
    } = Query::from(query);

    // If there is no fuzzy term, use the full query as the keyword,
    // otherwise restore the fuzzy query as the keyword we are going to search.
    let (keyword, query_type, usage_matcher) = if fuzzy_terms.is_empty() {
        if exact_terms.is_empty() {
            (query.into(), QueryType::StartWith, UsageMatcher::default())
        } else {
            (
                exact_terms[0].word.clone(),
                QueryType::Exact,
                UsageMatcher::new(exact_terms, inverse_terms),
            )
        }
    } else {
        (
            fuzzy_terms.iter().map(|term| &term.word).join(" "),
            QueryType::StartWith,
            UsageMatcher::new(exact_terms, inverse_terms),
        )
    };

    // TODO: Search syntax:
    // - 'foo
    // - foo*
    // - foo
    //
    // if let Some(stripped) = query.strip_suffix('*') {
    // (stripped, QueryType::Contain)
    // } else if let Some(stripped) = query.strip_prefix('\'') {
    // (stripped, QueryType::Exact)
    // } else {
    // (query, QueryType::StartWith)
    // };

    QueryInfo {
        keyword,
        query_type,
        usage_matcher,
    }
}

#[derive(Debug, Clone, Default)]
struct SearchResults {
    /// Last searching results.
    ///
    /// When passing the line content from Vim to Rust, the performance
    /// of Vim can become very bad because some lines are extremely long,
    /// we cache the last results on Rust to allow passing the line number
    /// from Vim later instead.
    usages: Usages,
    /// Last parsed query info.
    query_info: QueryInfo,
}

#[derive(Debug, Clone)]
pub struct DumbJumpProvider {
    /// Results from last searching.
    /// This might be a superset of searching results for the last query.
    cached_results: SearchResults,
    /// Current results from refiltering on `cached_results`.
    current_usages: Option<Usages>,
    /// Whether the tags file has been (re)-created.
    ctags_regenerated: Arc<AtomicBool>,
    /// Whether the GTAGS file has been (re)-created.
    gtags_regenerated: Arc<AtomicBool>,
}

impl DumbJumpProvider {
    pub fn new() -> Self {
        Self {
            cached_results: Default::default(),
            current_usages: None,
            ctags_regenerated: Arc::new(false.into()),
            gtags_regenerated: Arc::new(false.into()),
        }
    }

    async fn initialize_tags(&self, extension: String, cwd: AbsPathBuf) -> Result<()> {
        let job_id = utils::calculate_hash(&(&cwd, "dumb_jump"));

        if job::reserve(job_id) {
            let ctags_future = {
                let cwd = cwd.clone();
                let mut tags_generator = TagsGenerator::with_dir(cwd.clone());
                if let Some(language) = get_language(&extension) {
                    tags_generator.set_languages(language.into());
                }
                let ctags_regenerated = self.ctags_regenerated.clone();

                // TODO: smarter strategy to regenerate the tags?
                async move {
                    let now = std::time::Instant::now();
                    let ctags_searcher = CtagsSearcher::new(tags_generator);
                    match ctags_searcher.generate_tags() {
                        Ok(()) => {
                            ctags_regenerated.store(true, Ordering::SeqCst);
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, "💔 Error at generating the tags file for dumb_jump");
                        }
                    }
                    tracing::debug!(?cwd, "⏱️  Ctags elapsed: {:?}", now.elapsed());
                }
            };

            let gtags_future = {
                let gtags_regenerated = self.gtags_regenerated.clone();
                let span = tracing::span!(tracing::Level::INFO, "gtags");
                async move {
                    let gtags_searcher = GtagsSearcher::new(cwd.clone().into());
                    match tokio::task::spawn_blocking({
                        let gtags_searcher = gtags_searcher.clone();
                        move || gtags_searcher.create_or_update_tags()
                    })
                    .await
                    {
                        Ok(_) => gtags_regenerated.store(true, Ordering::SeqCst),
                        Err(e) => {
                            tracing::error!(error = ?e, "💔 Error at initializing GTAGS, attempting to recreate...");
                            // TODO: creating gtags may take 20s+ for large project
                            match tokio::task::spawn_blocking({
                                let gtags_searcher = gtags_searcher.clone();
                                move || gtags_searcher.force_recreate()
                            })
                            .await
                            {
                                Ok(_) => {
                                    gtags_regenerated.store(true, Ordering::SeqCst);
                                    tracing::debug!("Recreating gtags db successfully");
                                }
                                Err(e) => {
                                    tracing::error!(error = ?e, "💔 Failed to recreate gtags db");
                                }
                            }
                        }
                    }
                }.instrument(span)
            };

            fn run(job_future: impl Send + Sync + 'static + Future<Output = ()>, job_id: u64) {
                tokio::task::spawn({
                    async move {
                        let now = std::time::Instant::now();
                        job_future.await;
                        tracing::debug!("⏱️  Total elapsed: {:?}", now.elapsed());
                        job::unreserve(job_id);
                    }
                });
            }

            match (*CTAGS_EXISTS, *GTAGS_EXISTS) {
                (true, true) => run(
                    async move {
                        futures::future::join(ctags_future, gtags_future).await;
                    },
                    job_id,
                ),
                (false, false) => {}
                (true, false) => run(ctags_future, job_id),
                (false, true) => run(gtags_future, job_id),
            }
        }

        Ok(())
    }

    /// Starts a new searching task.
    async fn start_search(
        &self,
        searching_worker: SearchingWorker,
        query: String,
        query_info: QueryInfo,
        ctx: &Context,
    ) -> Result<SearchResults> {
        if query.is_empty() {
            return Ok(Default::default());
        }

        let search_engine = match (
            self.ctags_regenerated.load(Ordering::Relaxed),
            self.gtags_regenerated.load(Ordering::Relaxed),
        ) {
            (true, true) => SearchEngine::All,
            (true, false) => SearchEngine::CtagsAndRegex,
            _ => SearchEngine::Regex,
        };

        let (response, usages) = match search_engine.run(searching_worker).await {
            Ok(usages) => {
                let response = {
                    let total = usages.len();
                    // Only show the top 200 items.
                    let (lines, indices): (Vec<_>, Vec<_>) = usages
                        .iter()
                        .take(200)
                        .map(|usage| (usage.line.as_str(), usage.indices.as_slice()))
                        .unzip();
                    json!({ "lines": lines, "indices": indices, "total": total })
                };

                (response, usages)
            }
            Err(e) => {
                tracing::error!(error = ?e, "Error at running dumb_jump");
                let response = json!({ "error": { "message": e.to_string() } });
                (response, Default::default())
            }
        };

        ctx.vim
            .exec("clap#state#process_response_on_typed", response)?;

        Ok(SearchResults { usages, query_info })
    }
}

#[async_trait::async_trait]
impl ClapProvider for DumbJumpProvider {
    async fn on_create(&mut self, ctx: &mut Context) -> Result<()> {
        let cwd = ctx.vim.working_dir().await?;
        let extension = ctx.start_buffer_extension()?;

        tokio::task::spawn({
            let cwd = cwd.clone();
            let extension = extension.clone();
            let dumb_jump = self.clone();

            async move {
                if let Err(err) = dumb_jump.initialize_tags(extension, cwd).await {
                    tracing::error!(error = ?err, "Failed to initialize dumb_jump provider");
                }
            }
        });

        let query = ctx.vim.context_query_or_input().await?;
        if !query.is_empty() {
            let query_info = parse_query_info(&query);
            let searching_worker = SearchingWorker {
                cwd,
                query_info: query_info.clone(),
                source_file_extension: extension,
            };
            self.cached_results = self
                .start_search(searching_worker, query, query_info, ctx)
                .await?;
            self.current_usages.take();
        }

        Ok(())
    }

    async fn on_move(&mut self, ctx: &mut Context) -> Result<()> {
        let current_lines = self
            .current_usages
            .as_ref()
            .unwrap_or(&self.cached_results.usages);

        if current_lines.is_empty() {
            return Ok(());
        }

        let input = ctx.vim.input_get().await?;
        let lnum = ctx.vim.display_getcurlnum().await?;

        // lnum is 1-indexed
        let curline = current_lines
            .get_line(lnum - 1)
            .ok_or_else(|| anyhow::anyhow!("Can not find curline on Rust end for lnum: {lnum}"))?;

        let preview_height = ctx.preview_height().await?;
        let preview_impl = CachedPreviewImpl::new(curline.to_string(), preview_height, ctx)?;
        let preview = preview_impl.get_preview().await?;

        let current_input = ctx.vim.input_get().await?;
        let current_lnum = ctx.vim.display_getcurlnum().await?;
        // Only send back the result if the request is not out-dated.
        if input == current_input && lnum == current_lnum {
            ctx.render_preview(preview)?;
        }

        Ok(())
    }

    async fn on_typed(&mut self, ctx: &mut Context) -> Result<()> {
        let query = ctx.vim.input_get().await?;
        let query_info = parse_query_info(&query);

        // Try to refilter the cached results.
        if self.cached_results.query_info.is_superset(&query_info) {
            let refiltered = self
                .cached_results
                .usages
                .par_iter()
                .filter_map(|Usage { line, indices }| {
                    query_info
                        .usage_matcher
                        .match_jump_line((line.clone(), indices.clone()))
                        .map(|(line, indices)| Usage::new(line, indices))
                })
                .collect::<Vec<_>>();
            let total = refiltered.len();
            let (lines, indices): (Vec<&str>, Vec<&[usize]>) = refiltered
                .iter()
                .take(200)
                .map(|Usage { line, indices }| (line.as_str(), indices.as_slice()))
                .unzip();
            let response = json!({ "lines": lines, "indices": indices, "total": total });
            ctx.vim
                .exec("clap#state#process_response_on_typed", response)?;
            self.current_usages.replace(refiltered.into());
            return Ok(());
        }

        let cwd: AbsPathBuf = ctx.vim.working_dir().await?;
        let extension = ctx.start_buffer_extension()?;
        let searching_worker = SearchingWorker {
            cwd,
            query_info: query_info.clone(),
            source_file_extension: extension,
        };
        self.cached_results = self
            .start_search(searching_worker, query, query_info, ctx)
            .await?;
        self.current_usages.take();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_search_info() {
        let query_info = parse_query_info("'foo");
        println!("{:?}", query_info);
    }
}
