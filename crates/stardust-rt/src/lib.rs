//! # stardust-rt
//!
//! Real-time-safe primitives for the audio thread.
//!
//! Everything exposed here is safe to use from an audio callback:
//!
//! - **No allocations** — all storage is pre-allocated at construction time.
//! - **No locks** — uses lock-free SPSC ring buffers ([`rtrb`]) for inter-
//!   thread communication.
//! - **No syscalls** in the hot path.
//!
//! Anything that doesn't meet that bar does not belong in this crate.
//!
//! # SPSC ring buffer
//!
//! The core primitive for moving data from a non-audio thread (UI, MIDI input)
//! into the audio thread (or vice versa):
//!
//! ```
//! use stardust_rt::RingBuffer;
//!
//! // Pre-allocate space for up to 1024 events. No allocations after this.
//! let (mut producer, mut consumer) = RingBuffer::<u32>::new(1024);
//!
//! // Non-audio thread: push (fails if full, never blocks).
//! producer.push(42).expect("queue full");
//!
//! // Audio thread: pop (returns None if empty, never blocks).
//! assert_eq!(consumer.pop().ok(), Some(42));
//! ```
//!
//! `Producer` is `Send` but not `Sync` — it must be owned by exactly one
//! producer thread. Same for `Consumer`. Compiler enforces this.

#![doc(html_root_url = "https://docs.rs/stardust-rt/0.0.1")]
#![warn(missing_docs)]

pub use rtrb::{Consumer, PopError, Producer, PushError, RingBuffer};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_roundtrip() {
        let (mut p, mut c) = RingBuffer::<u32>::new(4);
        assert!(c.pop().is_err());
        p.push(1).unwrap();
        p.push(2).unwrap();
        p.push(3).unwrap();
        assert_eq!(c.pop().unwrap(), 1);
        assert_eq!(c.pop().unwrap(), 2);
        assert_eq!(c.pop().unwrap(), 3);
        assert!(c.pop().is_err());
    }

    #[test]
    fn returns_err_when_full() {
        let (mut p, _c) = RingBuffer::<u32>::new(2);
        p.push(1).unwrap();
        p.push(2).unwrap();
        // rtrb caps at the requested size minus one slot it reserves
        // internally; the third push must fail, never block.
        assert!(p.push(3).is_err());
    }

    #[test]
    fn cross_thread_drain_preserves_order() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (mut p, mut c) = RingBuffer::<u32>::new(2048);
        let done = Arc::new(AtomicBool::new(false));
        let done_w = done.clone();

        let producer = std::thread::spawn(move || {
            for i in 0..1000u32 {
                // Spin until the slot is free — POC test only.
                while p.push(i).is_err() {
                    std::hint::spin_loop();
                }
            }
            done_w.store(true, Ordering::Release);
        });

        let mut received = Vec::with_capacity(1000);
        while !(done.load(Ordering::Acquire) && c.is_empty()) {
            while let Ok(v) = c.pop() {
                received.push(v);
            }
            std::hint::spin_loop();
        }

        producer.join().unwrap();
        assert_eq!(received.len(), 1000);
        for (i, v) in received.iter().enumerate() {
            assert_eq!(*v as usize, i, "out-of-order or lost event at index {i}");
        }
    }
}
