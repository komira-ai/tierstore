//! `docs/metrics.md` held to `/metricz`: every series name the page names, in any section, is a
//! family the `/metricz` of a node with every source (a proxy, the OCI front and a local cache)
//! shows, and every family that `/metricz` shows is named by the page. The docs test checks the
//! page's names against the text of the source; this one checks them against what is rendered,
//! so a name that only a comment or a constant holds, or a family the page forgot, is found.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode};

use super::docs_tests::metrics_in;
use super::local_cache::testing::Rig as CacheRig;
use super::oci::OciMetrics;
use super::testing::Rig;

/// The page, as the crate ships it.
const PAGE: &str = include_str!("../../docs/metrics.md");

/// The families of a `/metricz` text: the name of each `# TYPE` line and of each sample.
fn families(metricz: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in metricz.lines() {
        let name = match line.strip_prefix("# TYPE ") {
            Some(typed) => typed.split(' ').next(),
            None if line.starts_with('#') => None,
            None => line.split(['{', ' ']).next(),
        };
        found.extend(name.filter(|name| !name.is_empty()).map(str::to_owned));
    }
    found
}

/// What differs between the page and a `/metricz` text: each name the page names that is not a
/// family of the text, and each family no line of the page names. A word that ends in `_` is a
/// prefix (`tierstore_cache_`), not a name.
fn drift(page: &str, metricz: &str) -> Vec<String> {
    let named: BTreeSet<String> = metrics_in(page)
        .into_iter()
        .filter(|word| !word.ends_with('_'))
        .collect();
    let shown = families(metricz);
    let unshown = named
        .difference(&shown)
        .map(|name| format!("the page names {name}, which /metricz does not show"));
    let unnamed = shown
        .difference(&named)
        .map(|name| format!("/metricz shows {name}, which the page does not name"));
    unshown.chain(unnamed).collect()
}

/// The `/metricz` of a server with a proxy, the OCI front and a local cache.
async fn every_block() -> String {
    let cache = CacheRig::new("tscache-metrics-page");
    let rig = Rig::with(1 << 20, |config| {
        config.oci_metrics = Some(Arc::new(OciMetrics::default()));
        config.cache = Some(cache.cache.clone());
    })
    .await;
    let request = Request::builder()
        .method("GET")
        .uri("/metricz")
        .body(Full::new(Bytes::new()))
        .expect("a request");
    let reply = rig.send_raw(request).await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.body
}

/// Asserts that every series name `docs/metrics.md` names, the sections of what is not in a
/// release yet and the PromQL included, is a family of `/metricz`, and that every family of
/// `/metricz` is named by the page. It fails on a page that documents a series no code emits and
/// on a series the code emits that the page leaves out.
#[tokio::test]
async fn the_metrics_page_names_exactly_the_families_of_metricz() {
    let metricz = every_block().await;
    for block in [
        "tierstore_hits_total",
        "tierstore_oci_front_info",
        "tierstore_cache_hits_total",
    ] {
        assert!(families(&metricz).contains(block), "{metricz}");
    }
    assert_eq!(drift(PAGE, &metricz), Vec::<String>::new());
}

/// Asserts that the comparison finds each kind of drift and nothing else: a name only the page
/// has, a family only `/metricz` has, and that it reads names from labels, `# TYPE` lines, PromQL
/// and prose while leaving out a prefix and a comment line.
#[test]
fn the_comparison_finds_a_name_on_either_side_alone() {
    let metricz = "# a comment naming tierstore_not_a_family\n\
                   # TYPE tierstore_a_total counter\n\
                   tierstore_a_total{op=\"get\"} 1\n\
                   tierstore_b 2\n";
    let page = "`tierstore_a_total` and rate(tierstore_b[5m]); every `tierstore_` name.";
    assert_eq!(drift(page, metricz), Vec::<String>::new());

    let extra = format!("{page} `tierstore_cache_upstream_reads_total{{op=\"get\"}}`");
    assert_eq!(
        drift(&extra, metricz),
        ["the page names tierstore_cache_upstream_reads_total, which /metricz does not show"]
    );
    let forgot = "`tierstore_a_total` alone";
    assert_eq!(
        drift(forgot, metricz),
        ["/metricz shows tierstore_b, which the page does not name"]
    );
    let typed_only = "# TYPE tierstore_c gauge\n";
    assert_eq!(
        families(typed_only),
        BTreeSet::from(["tierstore_c".to_owned()])
    );
}
