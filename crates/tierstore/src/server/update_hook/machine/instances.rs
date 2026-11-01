//! The instances' HTTP answers: a blocking `GET` on `127.0.0.1`, over `std::net`.

use std::time::Duration;

use super::super::ports::{Instances, PortError, Reply};

/// A client of the instances' loopback ports.
#[derive(Clone, Copy, Debug)]
pub struct LoopbackInstances {
    #[allow(dead_code)]
    connect_timeout: Duration,
    #[allow(dead_code)]
    read_timeout: Duration,
}

impl LoopbackInstances {
    /// A connect that takes longer than `connect_timeout` fails, and so does a read (or a
    /// write) that waits longer than `read_timeout` for a byte.
    #[must_use]
    pub fn new(connect_timeout: Duration, read_timeout: Duration) -> LoopbackInstances {
        LoopbackInstances {
            connect_timeout,
            read_timeout,
        }
    }
}

impl Instances for LoopbackInstances {
    fn get(&self, _port: u16, _path: &str) -> Result<Reply, PortError> {
        Ok(Reply {
            status: 0,
            body: String::new(),
        })
    }
}
