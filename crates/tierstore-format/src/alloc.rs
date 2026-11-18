//! The per-node allocation of block and stripe ids (10.2).
//!
//! A block id and a stripe id are `(allocating node: 8 bits, per-node counter: 56 bits)`, so a node
//! allocates its ids alone and no central allocator exists. An [`IdAllocator`] hands out a node's
//! counters in increasing order: one id at a time ([`IdAllocator::next`]), or a run of consecutive
//! ids ([`IdAllocator::next_run`]), which a chain needs (its slices are consecutive blocks).
//!
//! Counters start at 1: a counter of 0 is never allocated, so an id read from zeroed bytes never
//! names a real block or stripe.
//!
//! **After a restart** a node raises its allocators above every id of its own it finds (segment,
//! index, seal and stripe file names, the block table) with [`IdAllocator::observe`] before it
//! allocates, so no id is handed out twice. Ids of other nodes are ignored by `observe`: they are
//! another node's counters.
//!
//! The allocator is one `AtomicU64` and may be used from any number of threads; no two calls
//! return overlapping ids.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::FormatError;
use crate::ids::{BlockId, COUNTER_MAX, NodeId, StripeId};

/// An id a node allocates from its counter: [`BlockId`] or [`StripeId`].
pub trait AllocatedId: Copy + sealed::Sealed {
    /// The id of `node`'s `counter`.
    fn make(node: NodeId, counter: u64) -> Result<Self, FormatError>;
    /// The node that allocated the id.
    fn allocator(self) -> NodeId;
    /// The allocating node's counter.
    fn count(self) -> u64;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::ids::BlockId {}
    impl Sealed for crate::ids::StripeId {}
}

macro_rules! allocated {
    ($name:ident) => {
        impl AllocatedId for $name {
            fn make(node: NodeId, counter: u64) -> Result<Self, FormatError> {
                Self::new(node, counter)
            }

            fn allocator(self) -> NodeId {
                self.node()
            }

            fn count(self) -> u64 {
                self.counter()
            }
        }
    };
}

allocated!(BlockId);
allocated!(StripeId);

/// The allocator of one node's ids of one kind.
#[derive(Debug)]
pub struct IdAllocator<I> {
    node: NodeId,
    /// The next counter to hand out; at most `COUNTER_MAX + 1` (then every counter is used).
    next: AtomicU64,
    kind: PhantomData<fn() -> I>,
}

impl<I: AllocatedId> IdAllocator<I> {
    /// The allocator of `node`, whose first id has counter 1.
    #[must_use]
    pub const fn new(node: NodeId) -> Self {
        Self {
            node,
            next: AtomicU64::new(1),
            kind: PhantomData,
        }
    }

    /// The node whose ids this allocator hands out.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    /// Raises the allocator above `id` if this node allocated it; an id of another node is
    /// ignored. Never lowers it.
    pub fn observe(&self, id: I) {
        if id.allocator() == self.node {
            // A counter is at most `COUNTER_MAX`, so the sum does not overflow.
            self.next.fetch_max(id.count() + 1, Ordering::AcqRel);
        }
    }

    /// The next id. An error once the node's 2^56 - 1 counters are used.
    pub fn next(&self) -> Result<I, FormatError> {
        self.next_run(1)
    }

    /// The first of `n` consecutive ids (counters `c, c + 1, ..., c + n - 1`), all of them this
    /// call's. An error for `n` of 0, or when fewer than `n` counters are left (no counter is
    /// used then).
    pub fn next_run(&self, n: u64) -> Result<I, FormatError> {
        if n == 0 {
            return Err(FormatError::Invalid {
                what: "id allocator",
                reason: "a run of no ids",
            });
        }
        let bump = |next: u64| {
            next.checked_add(n)
                .filter(|after| *after <= COUNTER_MAX + 1)
        };
        match self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, bump)
        {
            Ok(first) => I::make(self.node, first),
            Err(_) => Err(FormatError::Invalid {
                what: "id allocator",
                reason: "the node's counters are used up",
            }),
        }
    }
}
