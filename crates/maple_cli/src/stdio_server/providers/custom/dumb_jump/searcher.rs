use std::path::Path;

use anyhow::Result;
use dumb_analyzer::resolve_reference_kind;
use itertools::Itertools;
use rayon::prelude::*;

use super::QueryInfo;
use crate::find_usages::{
    AddressableUsage, CtagsSearcher, GtagsSearcher, QueryType, RegexSearcher, Usage, Usages,
};
use crate::tools::ctags::{get_language, TagsGenerator};
use crate::utils::ExactOrInverseTerms;

/// Context for performing a search.
#[derive(Debug, Clone, Default)]
pub(super) struct SearchingWorker {
    pub cwd: String,
    pub query_info: QueryInfo,
    pub source_file_extension: String,
}

impl SearchingWorker {
    fn ctags_search(self) -> Result<Vec<AddressableUsage>> {
        let mut tags_generator = TagsGenerator::with_dir(self.cwd);
        if let Some(language) = get_language(&self.source_file_extension) {
            tags_generator.set_languages(language.into());
        }

        let QueryInfo {
            keyword,
            query_type,
            filtering_terms,
        } = self.query_info;

        CtagsSearcher::new(tags_generator).search_usages(
            &keyword,
            &filtering_terms,
            query_type,
            true,
        )
    }

    fn gtags_search(self) -> Result<Vec<AddressableUsage>> {
        let QueryInfo {
            keyword,
            filtering_terms,
            ..
        } = self.query_info;
        GtagsSearcher::new(self.cwd.into()).search_usages(
            &keyword,
            &filtering_terms,
            &self.source_file_extension,
        )
    }

    fn regex_search(self) -> Result<Vec<AddressableUsage>> {
        let QueryInfo {
            keyword,
            filtering_terms,
            ..
        } = self.query_info;
        let searcher = RegexSearcher {
            word: keyword,
            extension: self.source_file_extension,
            dir: Some(self.cwd.into()),
        };
        searcher.search_usages(false, &filtering_terms)
    }
}

/// Returns a combo of various results in the order of [ctags, gtags, regex].
fn merge_all(
    ctag_results: Vec<AddressableUsage>,
    maybe_gtags_results: Option<Vec<AddressableUsage>>,
    regex_results: Vec<AddressableUsage>,
) -> Vec<AddressableUsage> {
    let mut regex_results = regex_results;
    regex_results.retain(|r| !ctag_results.contains(r));

    let mut results = ctag_results;
    if let Some(mut gtags_results) = maybe_gtags_results {
        regex_results.retain(|r| !gtags_results.contains(r));
        results.append(&mut gtags_results);
    }

    results.append(&mut regex_results);

    results
}

/// These is no best option here, each search engine has its own advantages and
/// disadvantages, hence, we make use of all of them to achieve a comprehensive
/// result.
///
/// # Comparison between all the search engines
///
/// |                | Ctags | Gtags                     | Regex                        |
/// | ----           | ----  | ----                      | ----                         |
/// | Initialization | No    | Required                  | No                           |
/// | Create         | Fast  | Slow                      | Fast                         |
/// | Update         | Fast  | Fast                      | Fast                         |
/// | Support        | Defs  | Defs(unpolished) and refs | Defs and refs(less accurate) |
///
/// The initialization of Ctags for a new project is normally
/// faster than Gtags, but once Gtags has been initialized,
/// the incremental update of Gtags should be instant enough
/// and is comparable to Ctags regarding the speed.
///
/// Regex requires no initialization.
#[derive(Debug, Clone)]
pub(super) enum SearchEngine {
    Ctags,
    Regex,
    CtagsAndRegex,
    CtagsElseRegex,
    All,
}

impl SearchEngine {
    pub async fn run(&self, searching_worker: SearchingWorker) -> Result<Usages> {
        let ctags_future = {
            let searching_worker = searching_worker.clone();
            async move { searching_worker.ctags_search() }
        };

        let regex_future = {
            let searching_worker = searching_worker.clone();
            async move { searching_worker.regex_search() }
        };

        let addressable_usages = match self {
            SearchEngine::Ctags => searching_worker.ctags_search()?,
            SearchEngine::Regex => searching_worker.regex_search()?,
            SearchEngine::CtagsAndRegex => {
                let (ctags_results, regex_results) = futures::join!(ctags_future, regex_future);

                merge_all(
                    ctags_results.unwrap_or_default(),
                    None,
                    regex_results.unwrap_or_default(),
                )
            }
            SearchEngine::CtagsElseRegex => {
                let results = searching_worker.clone().ctags_search();
                // tags might be incomplete, try the regex way if no results from the tags file.
                let try_regex =
                    results.is_err() || results.as_ref().map(|r| r.is_empty()).unwrap_or(false);
                if try_regex {
                    searching_worker.regex_search()?
                } else {
                    results?
                }
            }
            SearchEngine::All => {
                let gtags_future = {
                    let searching_worker = searching_worker.clone();
                    async move { searching_worker.gtags_search() }
                };

                let (ctags_results, gtags_results, regex_results) =
                    futures::join!(ctags_future, gtags_future, regex_future);

                merge_all(
                    ctags_results.unwrap_or_default(),
                    gtags_results.ok(),
                    regex_results.unwrap_or_default(),
                )
            }
        };

        Ok(addressable_usages.into())
    }
}
