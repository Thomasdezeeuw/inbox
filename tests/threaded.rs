//! Tests using multiple threads.

#![feature(once_cell)]

use std::thread;
use std::time::Duration;

use inbox::{new, RecvError, SendError};

#[macro_use]
mod util;

#[test]
#[cfg_attr(miri, ignore)] // Doesn't finish.
fn send_single_value() {
    with_all_capacities!(|capacity| {
        let (sender, mut receiver) = new::<usize>(capacity);

        start_threads!(
            {
                expect_send!(sender, 1);
            },
            {
                expect_recv!(receiver, 1);
            }
        );
    });
}

#[test]
#[cfg_attr(miri, ignore)] // Doesn't finish.
fn zero_sized_types() {
    with_all_capacities!(|capacity| {
        let (sender, mut receiver) = new(capacity);

        start_threads!(
            {
                expect_send!(sender, ());
            },
            {
                expect_recv!(receiver, ());
            }
        );
    });
}

#[test]
#[cfg_attr(miri, ignore)] // Doesn't finish.
fn receive_no_sender() {
    with_all_capacities!(|capacity| {
        let (sender, mut receiver) = new::<usize>(capacity);

        start_threads!(
            {
                drop(sender);
            },
            {
                r#loop! {
                    match receiver.try_recv() {
                        Ok(..) => panic!("unexpected receive of value"),
                        Err(RecvError::Empty) => {} // Try again.
                        Err(RecvError::Disconnected) => break,
                    }
                }
            }
        );
    });
}

#[test]
#[cfg_attr(miri, ignore)] // Doesn't support `sleep`.
fn send_no_receiver() {
    with_all_capacities!(|capacity| {
        let (sender, receiver) = new::<usize>(capacity);

        start_threads!(
            {
                thread::sleep(Duration::from_millis(1));
                r#loop! {
                    match sender.try_send(1) {
                        Ok(()) => {} // Try again.
                        Err(SendError::Full(..)) => panic!("too slow!"),
                        Err(SendError::Disconnected(..)) => break,
                    }
                }
            },
            {
                drop(receiver);
            }
        );
    });
}

#[test]
fn sender_is_connected() {
    with_all_capacities!(|capacity| {
        let (sender, receiver) = new::<usize>(capacity);

        start_threads!(
            {
                r#loop! {
                    if !sender.is_connected() {
                        break;
                    }
                }
            },
            {
                drop(receiver);
            }
        );
    });
}

#[test]
#[cfg_attr(miri, ignore)] // Doesn't finish.
fn receiver_is_connected() {
    with_all_capacities!(|capacity| {
        let (sender, receiver) = new::<usize>(capacity);

        start_threads!(
            {
                drop(sender);
            },
            {
                r#loop! {
                    if !receiver.is_connected() {
                        break;
                    }
                }
            }
        );
    });
}
