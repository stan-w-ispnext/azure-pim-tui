use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::jira::JiraIssue;

/// How long a Jira answer is reused before asking again.
const TTL: Duration = Duration::from_secs(5 * 60);
/// Upper bound on cached single-issue lookups.
const MAX_ISSUES: usize = 50;

/// Short-lived cache of Jira answers, plus the requests currently in flight so
/// the same question is never asked twice at once. Failures are not cached.
#[derive(Default)]
pub struct JiraCache {
    /// Single-issue lookups by key; `None` records that the issue doesn't exist.
    issues: HashMap<String, (Instant, Option<JiraIssue>)>,
    /// Result of the in-progress search.
    search: Option<(Instant, Vec<JiraIssue>)>,
    issues_in_flight: HashSet<String>,
    search_in_flight: bool,
}

/// What to do about a request.
#[derive(Debug, PartialEq)]
pub enum Lookup<T> {
    /// Fresh cached answer.
    Cached(T),
    /// Not cached; the caller should send the request (now marked in flight).
    Fetch,
    /// Already being fetched; its result will arrive on its own.
    InFlight,
}

impl JiraCache {
    pub fn issue(&mut self, key: &str) -> Lookup<Option<JiraIssue>> {
        self.issue_at(key, Instant::now())
    }

    /// Fresh cached answer for `key`, without starting a request.
    pub fn peek_issue(&self, key: &str) -> Option<Option<JiraIssue>> {
        self.peek_issue_at(key, Instant::now())
    }

    pub fn store_issue(&mut self, key: &str, result: Result<Option<JiraIssue>, ()>) {
        self.store_issue_at(key, result, Instant::now());
    }

    pub fn search(&mut self) -> Lookup<Vec<JiraIssue>> {
        self.search_at(Instant::now())
    }

    pub fn store_search(&mut self, result: Result<Vec<JiraIssue>, ()>) {
        self.store_search_at(result, Instant::now());
    }

    fn peek_issue_at(&self, key: &str, now: Instant) -> Option<Option<JiraIssue>> {
        self.issues
            .get(key)
            .filter(|(at, _)| now.duration_since(*at) < TTL)
            .map(|(_, issue)| issue.clone())
    }

    fn issue_at(&mut self, key: &str, now: Instant) -> Lookup<Option<JiraIssue>> {
        match self.issues.get(key) {
            Some((at, issue)) if now.duration_since(*at) < TTL => {
                return Lookup::Cached(issue.clone())
            }
            Some(_) => {
                self.issues.remove(key);
            }
            None => {}
        }
        if self.issues_in_flight.insert(key.to_string()) {
            Lookup::Fetch
        } else {
            Lookup::InFlight
        }
    }

    fn store_issue_at(&mut self, key: &str, result: Result<Option<JiraIssue>, ()>, now: Instant) {
        self.issues_in_flight.remove(key);
        let Ok(issue) = result else {
            return;
        };
        if !self.issues.contains_key(key) && self.issues.len() >= MAX_ISSUES {
            self.issues
                .retain(|_, (at, _)| now.duration_since(*at) < TTL);
            if self.issues.len() >= MAX_ISSUES {
                if let Some(oldest) = self
                    .issues
                    .iter()
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(k, _)| k.clone())
                {
                    self.issues.remove(&oldest);
                }
            }
        }
        self.issues.insert(key.to_string(), (now, issue));
    }

    fn search_at(&mut self, now: Instant) -> Lookup<Vec<JiraIssue>> {
        match &self.search {
            Some((at, issues)) if now.duration_since(*at) < TTL => {
                return Lookup::Cached(issues.clone())
            }
            Some(_) => self.search = None,
            None => {}
        }
        if self.search_in_flight {
            Lookup::InFlight
        } else {
            self.search_in_flight = true;
            Lookup::Fetch
        }
    }

    fn store_search_at(&mut self, result: Result<Vec<JiraIssue>, ()>, now: Instant) {
        self.search_in_flight = false;
        if let Ok(issues) = result {
            self.search = Some((now, issues));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(key: &str) -> JiraIssue {
        JiraIssue {
            key: key.to_string(),
            summary: format!("summary of {key}"),
        }
    }

    #[test]
    fn issue_fetched_once_then_cached() {
        let mut cache = JiraCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.issue_at("A-1", t0), Lookup::Fetch);
        assert_eq!(cache.issue_at("A-1", t0), Lookup::InFlight);
        cache.store_issue_at("A-1", Ok(Some(issue("A-1"))), t0);
        assert_eq!(
            cache.issue_at("A-1", t0 + Duration::from_secs(60)),
            Lookup::Cached(Some(issue("A-1")))
        );
    }

    #[test]
    fn peek_returns_fresh_entries_without_marking_in_flight() {
        let mut cache = JiraCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.peek_issue_at("A-1", t0), None);
        assert_eq!(cache.issue_at("A-1", t0), Lookup::Fetch);
        cache.store_issue_at("A-1", Ok(Some(issue("A-1"))), t0);
        assert_eq!(cache.peek_issue_at("A-1", t0), Some(Some(issue("A-1"))));
        assert_eq!(cache.peek_issue_at("A-1", t0 + TTL), None);
        assert_eq!(cache.peek_issue_at("A-2", t0), None);
        assert!(cache.issues_in_flight.is_empty());
    }

    #[test]
    fn not_found_is_cached() {
        let mut cache = JiraCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.issue_at("A-404", t0), Lookup::Fetch);
        cache.store_issue_at("A-404", Ok(None), t0);
        assert_eq!(cache.issue_at("A-404", t0), Lookup::Cached(None));
    }

    #[test]
    fn issue_expires_after_ttl() {
        let mut cache = JiraCache::default();
        let t0 = Instant::now();
        cache.issue_at("A-1", t0);
        cache.store_issue_at("A-1", Ok(Some(issue("A-1"))), t0);
        assert_eq!(cache.issue_at("A-1", t0 + TTL), Lookup::Fetch);
    }

    #[test]
    fn failure_is_not_cached_and_allows_retry() {
        let mut cache = JiraCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.issue_at("A-1", t0), Lookup::Fetch);
        cache.store_issue_at("A-1", Err(()), t0);
        assert_eq!(cache.issue_at("A-1", t0), Lookup::Fetch);
    }

    #[test]
    fn issue_cache_is_bounded_and_evicts_oldest() {
        let mut cache = JiraCache::default();
        let t0 = Instant::now();
        for i in 0..=MAX_ISSUES {
            let key = format!("A-{i}");
            let at = t0 + Duration::from_millis(i as u64);
            cache.issue_at(&key, at);
            cache.store_issue_at(&key, Ok(None), at);
        }
        assert_eq!(cache.issues.len(), MAX_ISSUES);
        assert!(!cache.issues.contains_key("A-0"));
        assert!(cache.issues.contains_key(&format!("A-{MAX_ISSUES}")));
    }

    #[test]
    fn search_fetched_once_then_cached_until_ttl() {
        let mut cache = JiraCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.search_at(t0), Lookup::Fetch);
        assert_eq!(cache.search_at(t0), Lookup::InFlight);
        cache.store_search_at(Ok(vec![issue("A-1")]), t0);
        assert_eq!(cache.search_at(t0), Lookup::Cached(vec![issue("A-1")]));
        assert_eq!(cache.search_at(t0 + TTL), Lookup::Fetch);
    }

    #[test]
    fn search_failure_allows_retry() {
        let mut cache = JiraCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.search_at(t0), Lookup::Fetch);
        cache.store_search_at(Err(()), t0);
        assert_eq!(cache.search_at(t0), Lookup::Fetch);
    }
}
