//! The text of `/metricz` (the module docs of `server` say what it holds).

use super::{Server, build_info};

impl Server {
    /// The text of `/metricz`: the block of every source of counters the process has (the
    /// proxy's, the OCI front's, the local cache's), one after the other, each ending in a
    /// newline, or one line that names them all when there is none; then the late requests of a
    /// drain (`tierstore_drain_late_requests_total`), the families of the front (`bounds.rs`),
    /// the figures of the process (`process.rs`, none unless the server was given a source of
    /// them), and, always last, `tierstore_build_info` ([`build_info::gauge`]). Every block that
    /// exists renders, never one of them; a storage node has the proxy's, a cache node the
    /// cache's, and either may have the OCI front's.
    pub(super) fn metrics_probe(&self) -> String {
        let blocks = [
            self.config.proxy.as_ref().map(|proxy| proxy.metrics()),
            self.config.oci_metrics.as_ref().map(|oci| oci.render()),
            self.config.cache.as_ref().map(|cache| cache.metrics()),
        ];
        let mut text = String::new();
        for block in blocks.into_iter().flatten() {
            text.push_str(&block);
            if !block.ends_with('\n') {
                text.push('\n');
            }
        }
        if text.is_empty() {
            text = "# this server has no proxy, no OCI read front and no local cache\n".to_owned();
        }
        let late = self.late_requests();
        text.push_str("# TYPE tierstore_drain_late_requests_total counter\n");
        text.push_str(&format!("tierstore_drain_late_requests_total {late}\n"));
        text.push_str(&self.front_families());
        text.push_str(&self.process.read().render());
        let (version, commit) = (build_info::VERSION, build_info::COMMIT);
        text.push_str(&build_info::gauge(version, commit, self.config.slot));
        text
    }
}
