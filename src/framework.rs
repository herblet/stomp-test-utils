use std::{convert::TryInto, pin::Pin, time::Duration};

use futures::{future::join, Future, FutureExt};
use tokio::{
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    task::yield_now,
};

pub trait ErrorType: Send + std::fmt::Debug + 'static {}

impl<T: Send + std::fmt::Debug + 'static> ErrorType for T {}

pub type InSender<E> = UnboundedSender<Result<Vec<u8>, E>>;
pub type InReceiver<E> = UnboundedReceiver<Result<Vec<u8>, E>>;

pub type OutReceiver = UnboundedReceiver<Vec<u8>>;
pub type OutSender = UnboundedSender<Vec<u8>>;

/// Creates a session which receives and sends messages on the provided receiver and sender respectively.
pub trait SessionFactory<E: ErrorType>:
    FnOnce(InReceiver<E>, OutSender) -> Pin<Box<dyn Future<Output = Result<(), E>> + Send>>
{
}

impl<
        E: ErrorType,
        F: FnOnce(InReceiver<E>, OutSender) -> Pin<Box<dyn Future<Output = Result<(), E>> + Send>>,
    > SessionFactory<E> for F
{
}

/// A `BehaviourFunction` can send messages to the provided sender and check responses on the provided receiver,
/// thereby testing expected behaviour. It returns the channels it received as inputs in order to faciliate
/// further checks downstream, and enable chaining.
///
/// DOes
/// A blanket implementation for any Function with the appropriate Signature is provided.
pub trait BehaviourFunction<E: ErrorType>:
    for<'a> FnOnce(
        &'a mut InSender<E>,
        &'a mut OutReceiver,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
    + Send
{
}

impl<E: ErrorType, T> BehaviourFunction<E> for T where
    for<'a> T: FnOnce(
            &'a mut InSender<E>,
            &'a mut OutReceiver,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
        + Send
{
}

/// Enables chaining of [`BehaviourFunction`]s.
pub trait Chainable<E: ErrorType>: BehaviourFunction<E> + Sized {
    /// Constructs a new [`BehaviourFunction`] which will first execute `self`, and then `followed_by`.
    fn then<S: BehaviourFunction<E> + 'static>(
        self,
        followed_by: S,
    ) -> Box<dyn BehaviourFunction<E>>;
}

impl<E: ErrorType, T: BehaviourFunction<E> + 'static> Chainable<E> for T {
    fn then<S: BehaviourFunction<E> + 'static>(
        self,
        followed_by: S,
    ) -> Box<dyn BehaviourFunction<E>> {
        Box::new(|sender: &mut InSender<E>, receiver: &mut OutReceiver| {
            async move {
                self(sender, receiver).await;
                followed_by(sender, receiver).await
            }
            .boxed()
        })
    }
}

/// Help the compiler to assign appropriate lifetimes to inputs and outputs of BehaviourFunction-equivalent closure.
pub fn into_behaviour<E, C>(closure: C) -> impl BehaviourFunction<E>
where
    E: ErrorType,
    C: for<'a> FnOnce(
            &'a mut InSender<E>,
            &'a mut OutReceiver,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
        + Send
        + 'static,
{
    closure
}

/// The core function of this crate, it can be used to test the session returned by `session_factory` by executing
/// `behaviour` against it - in other words, the [`InSender`] provided to `behaviour` will send messages to the created
/// session, and the [`OutReceiver`] will receive messages the session sends to its 'counterparty'. Any errors
/// returned by either side will result in a panic.
pub async fn assert_behaviour<
    E: ErrorType,
    F: SessionFactory<E>,
    T: BehaviourFunction<E> + 'static,
>(
    session_factory: F,
    behaviour: T,
) {
    let (in_sender, in_receiver) = unbounded_channel::<Result<Vec<u8>, E>>();
    let (out_sender, out_receiver) = unbounded_channel();

    let session_future = session_factory(in_receiver, out_sender);

    let other_future = tokio::task::spawn(async move {
        let mut out_receiver = out_receiver;
        let mut in_sender = in_sender;

        behaviour(&mut in_sender, &mut out_receiver).await;
        out_receiver.close();
        drop(in_sender);
        ()
    });

    let results = join(session_future, other_future).await;

    results.0.expect("Session returned error");
    results.1.expect("Behaviour returned error");
}

/// Returns a [`BehaviourFuntion`] which sends the provided `data`, and then yields.
pub fn send<T: Into<Vec<u8>> + Send + 'static, E: ErrorType>(
    data: T,
) -> impl BehaviourFunction<E> {
    into_behaviour(move |in_sender: &mut InSender<E>, _: &mut OutReceiver| {
        send_data(in_sender, data);
        yield_now().boxed()
    })
}

/// Sends `data` via `sender`, after converting it to bytes and transforming any error using `from`. Panics
/// it the send fails.
pub fn send_data<T: Into<Vec<u8>>, E: ErrorType>(
    sender: &InSender<E>,
    data: T,
) {
    sender
        .send(Ok(data.into()))
        .expect("Send failed");
}

/// Asserts that the receiver can _immediately_ provide a message which passes
// the provided `predicate`. Thus the message must already have been sent by the session being tested.
pub fn assert_receive<T: FnOnce(Vec<u8>) -> bool>(out_receiver: &mut OutReceiver, predicate: T) {
    let response = out_receiver.recv().now_or_never();

    assert!(predicate(
        response
            .expect("No message from session") // Now or never was 'never'
            .expect("Session closed") // Message on channel was 'None'
    ));
}

/// Returns a [`BehaviourFunction`] which will assert that a message is waiting, and that that
/// message matches the provided `predicate`.
pub fn receive<E: ErrorType, T: FnOnce(Vec<u8>) -> bool + Send + 'static>(
    predicate: T,
) -> impl BehaviourFunction<E> {
    into_behaviour(|_: &mut InSender<E>, out_receiver: &mut OutReceiver| {
        async move {
            yield_now().await;
            assert_receive(out_receiver, predicate)
        }
        .boxed()
    })
}

/// Pauses tokio, sleeps for `millis` milliseconds, and then resumes tokio. Allows testing of actions that occur
/// after some time, such as heartbeats, without actually having to wait for that amount of time.
pub fn sleep_in_pause(millis: u64) -> impl Future<Output = ()> {
    tokio::time::pause();
    tokio::time::sleep(Duration::from_millis(millis)).inspect(|_| tokio::time::resume())
}

pub fn wait_for_disconnect<'a, E: ErrorType>(
    _: &'a mut InSender<E>,
    out_receiver: &'a mut OutReceiver,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    async move {
        sleep_in_pause(5050).await;

        assert!(matches!(out_receiver.recv().now_or_never(), Some(None)));
        ()
    }
    .boxed()
}

#[cfg(test)]
mod test {

    use std::{
        any::Any,
        convert::Infallible,
        panic::AssertUnwindSafe,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    use std::panic;

    use tokio::join;

    use super::*;

    #[tokio::test]
    async fn chaining_works() {
        let (mut tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        let behaviour_a = into_behaviour(|sender: &mut InSender<()>, _: &mut OutReceiver| {
            async move {
                sender.send(Ok(b"foo".to_vec())).expect("Send failed");
                ()
            }
            .boxed()
        });

        let behaviour_b = into_behaviour(|sender: &mut InSender<()>, _: &mut OutReceiver| {
            async move {
                sender.send(Ok(b"bar".to_vec())).expect("Send failed");
                ()
            }
            .boxed()
        });

        // a then b...
        behaviour_a.then(behaviour_b)(&mut tx, &mut out_rx).await;

        // ... then close the channel
        drop(tx);

        assert_eq!(
            "foo",
            String::from_utf8(
                rx.recv()
                    .now_or_never()
                    .unwrap()
                    .unwrap()
                    .expect("recv failed"),
            )
            .expect("Parse failed"),
        );
        assert_eq!(
            "bar",
            String::from_utf8(
                rx.recv()
                    .now_or_never()
                    .unwrap()
                    .unwrap()
                    .expect("recv failed"),
            )
            .expect("Parse failed"),
        );

        assert_eq!(None, rx.recv().now_or_never().unwrap());
    }

    #[derive(Debug, PartialEq, Eq)]
    struct TestError;

    impl From<Infallible> for TestError {
        fn from(_: Infallible) -> Self {
            TestError
        }
    }

    #[tokio::test]
    async fn send_works() {
        let (mut tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        let behaviour = send::<String, TestError>("Hello".to_owned());

        behaviour(&mut tx, &mut out_rx);

        // ... then close the channel
        drop(tx);

        assert_eq!(
            "Hello",
            String::from_utf8(
                rx.recv()
                    .now_or_never()
                    .unwrap()
                    .unwrap()
                    .expect("recv failed"),
            )
            .expect("Parse failed"),
        );

        assert_eq!(None, rx.recv().now_or_never().unwrap());
    }

    struct TestData;

    #[derive(Debug, PartialEq, Eq)]
    struct TestError2;

    impl From<TestError2> for TestError {
        fn from(_: TestError2) -> Self {
            TestError
        }
    }
    impl Into<Vec<u8>> for TestData {
        
        fn into(self) -> Vec<u8> {
            vec![]
        }
    }

    #[tokio::test]
    async fn send_handles_conversion_error() {
        let (mut tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        let behaviour = send::<TestData, TestError>(TestData);

        behaviour(&mut tx, &mut out_rx);

        // ... then close the channel
        drop(tx);

        assert_eq!(Err(TestError), rx.recv().now_or_never().unwrap().unwrap());

        assert_eq!(None, rx.recv().now_or_never().unwrap());
    }

    #[tokio::test]
    async fn send_yields() {
        let (mut tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<Vec<u8>, TestError>>();
        let (_, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        let handle = tokio::task::spawn(async move {
            let first = rx.recv().await;

            assert_eq!(
                "Hello",
                String::from_utf8(first.unwrap().expect("recv failed"),).expect("Parse failed"),
            );

            // Because the first send yielded to this task, the second one has not send yet
            assert_eq!(None, rx.recv().now_or_never());

            let second = rx.recv().await;

            assert_eq!(
                "world",
                String::from_utf8(second.unwrap().expect("recv failed"),).expect("Parse failed"),
            );
        });

        let behaviour = send::<String, TestError>("Hello".to_owned())
            .then(send::<String, TestError>("world".to_owned()));

        let x = join!(handle, behaviour(&mut tx, &mut out_rx));

        assert!(x.0.is_ok());
    }

    #[tokio::test]
    async fn send_data_sends() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<Vec<u8>, TestError>>();

        send_data(&tx, b"1".to_vec());

        assert_eq!(
            b"1".to_vec(),
            rx.recv()
                .await
                .expect("Should be Some")
                .expect("Should be ok")
        );
    }

    #[tokio::test]
    async fn test_expectations_executes_behaviour() {
        let called = Arc::new(AtomicBool::new(false));

        let session_factory = |_: InReceiver<TestError>, _: OutSender| async { Ok(()) }.boxed();
        let client_behaviour = into_behaviour({
            let called = called.clone();
            |_, _| async move { called.store(true, Ordering::Release) }.boxed()
        });

        assert_behaviour(session_factory, client_behaviour).await;

        assert_eq!(true, called.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn test_expectations_starts_session() {
        let called = Arc::new(AtomicBool::new(false));

        let session_factory = {
            let called = called.clone();
            |_: InReceiver<TestError>, _: OutSender| {
                async move { Ok(called.store(true, Ordering::Release)) }.boxed()
            }
        };
        let client_behaviour = into_behaviour(|_, _| async {}.boxed());

        assert_behaviour(session_factory, client_behaviour).await;

        assert_eq!(true, called.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn test_expectations_sends_in_to_session() {
        let session_factory = {
            |mut receiver: InReceiver<TestError>, _: OutSender| {
                async move {
                    match receiver.recv().await {
                        Some(Ok(x)) if x == vec![42u8] => Ok(()),
                        _ => Err(TestError),
                    }
                }
                .boxed()
            }
        };
        let client_behaviour = into_behaviour(|in_sender, _| {
            async move {
                in_sender.send(Ok(vec![42u8])).expect("Send failed");
            }
            .boxed()
        });

        assert_behaviour(session_factory, client_behaviour).await;
    }

    #[tokio::test]
    async fn test_expectations_receives_out_from_session() {
        let session_factory = {
            |_, sender: OutSender| {
                async move { sender.send(vec![42u8]).map_err(|_| TestError) }.boxed()
            }
        };
        let client_behaviour =
            into_behaviour::<TestError, _>(|_, out_receiver: &mut OutReceiver| {
                async move {
                    assert!(matches!(out_receiver.recv().await, Some(x) if x == vec![42u8]));
                }
                .boxed()
            });

        assert_behaviour(session_factory, client_behaviour).await;
    }

    fn assert_unwind_safe<O, F: Future<Output = O>>(
        future: F,
    ) -> impl Future<Output = Result<O, Box<dyn Any + std::marker::Send>>> {
        panic::set_hook(Box::new(|_info| {
            // do nothing
        }));

        AssertUnwindSafe(future).catch_unwind()
    }

    async fn assert_beaviour_test_result<
        E: ErrorType,
        F: SessionFactory<E> + 'static,
        B: BehaviourFunction<E> + 'static,
    >(
        session_factory: F,
        behaviour: B,
        expect_err: bool,
    ) {
        panic::set_hook(Box::new(|_info| {
            // do nothing
        }));
        let result = assert_unwind_safe(assert_behaviour(session_factory, behaviour)).await;

        assert_eq!(expect_err, result.is_err());
    }

    async fn assert_beaviour_test_fails<
        E: ErrorType,
        F: SessionFactory<E> + 'static,
        B: BehaviourFunction<E> + 'static,
    >(
        session_factory: F,
        behaviour: B,
    ) {
        assert_beaviour_test_result(session_factory, behaviour, true).await;
    }

    async fn assert_beaviour_test_succeeds<
        E: ErrorType,
        F: SessionFactory<E> + 'static,
        B: BehaviourFunction<E> + 'static,
    >(
        session_factory: F,
        behaviour: B,
    ) {
        assert_beaviour_test_result(session_factory, behaviour, false).await;
    }

    #[tokio::test]
    async fn test_expectations_fails_if_error_in_session() {
        let session_factory = { |_, _| async move { Err(TestError) }.boxed() };
        let client_behaviour = into_behaviour::<TestError, _>(|_, _| async move {}.boxed());

        assert_beaviour_test_fails(session_factory, client_behaviour).await;
    }

    #[tokio::test]
    async fn test_expectations_fails_if_error_in_behaviour() {
        let session_factory = |_, _| async move { Ok(()) }.boxed();
        let client_behaviour = into_behaviour::<TestError, _>(|_, _| {
            async move {
                assert!(false);
            }
            .boxed()
        });

        assert_beaviour_test_fails(session_factory, client_behaviour).await;
    }

    #[tokio::test]
    async fn test_expectations_succeeds_if_empty() {
        let session_factory = |_, _| async move { Ok(()) }.boxed();
        let client_behaviour = into_behaviour::<TestError, _>(|_, _| async move {}.boxed());

        assert_beaviour_test_succeeds(session_factory, client_behaviour).await;
    }

    #[tokio::test]
    async fn assert_receive_fails_if_no_message() {
        let (_, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        let result = assert_unwind_safe(async { assert_receive(&mut out_rx, |_| true) }).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn assert_receive_succeeds_if_message_matches() {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        out_tx.send(vec![0u8]).expect("Should succeed");
        let result = assert_unwind_safe(async { assert_receive(&mut out_rx, |_| true) }).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn assert_receive_succeeds_if_session_closed() {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        out_tx.send(vec![0u8]).expect("Should succeed");
        out_rx.close();

        let result = assert_unwind_safe(async { assert_receive(&mut out_rx, |_| true) }).await;

        assert!(result.is_ok());

        let result = assert_unwind_safe(async { assert_receive(&mut out_rx, |_| true) }).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn assert_receive_fails_if_predicate_false() {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        out_tx.send(vec![0u8]).expect("Should succeed");

        let result = assert_unwind_safe(async { assert_receive(&mut out_rx, |_| false) }).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn assert_receive_passes_message_to_predicate() {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        out_tx.send(vec![0u8]).expect("Should succeed");

        let result =
            assert_unwind_safe(async { assert_receive(&mut out_rx, |data| data == vec![0u8]) })
                .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn receive_succeeds_with_message() {
        let (mut tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        let behaviour = receive::<TestError, _>(|bytes| bytes == vec![42u8]);

        out_tx.send(vec![42u8]).expect("Send Failed");

        behaviour(&mut tx, &mut out_rx).await;
    }

    #[tokio::test]
    async fn receive_fails_with_incorrect_message() {
        let (mut tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        let behaviour = receive::<TestError, _>(|bytes| bytes == vec![43u8]);

        out_tx.send(vec![42u8]).expect("Send Failed");

        assert_unwind_safe(behaviour(&mut tx, &mut out_rx))
            .await
            .expect_err("Behaviour passed");
    }

    #[tokio::test]
    async fn sleep_in_pause_passes_time() {
        let session_factory = |_, sender: OutSender| {
            async move {
                tokio::time::sleep(Duration::from_millis(2000)).await;
                sender.send(vec![1]).expect("Send failed");
                Ok(())
            }
            .boxed()
        };

        let client_behaviour =
            into_behaviour::<TestError, _>(|_, out_receiver: &mut OutReceiver| {
                async move {
                    assert!(out_receiver.recv().now_or_never().is_none());
                    sleep_in_pause(3000).await;
                    assert_receive(out_receiver, |bytes| bytes == vec![1]);
                }
                .boxed()
            });

        assert_beaviour_test_succeeds(session_factory, client_behaviour).await;
    }

    #[tokio::test]
    async fn wait_for_disconnect_fails_if_not_disconnected() {
        let session_factory = |_, unused: OutSender| {
            async move {
                tokio::time::sleep(Duration::from_millis(5060)).await;
                drop(unused); // disconnects, but too late
                Ok(())
            }
            .boxed()
        };

        assert_beaviour_test_fails(session_factory, wait_for_disconnect::<TestError>).await;
    }

    #[tokio::test]
    async fn wait_for_disconnect_succeeds_if_disconnected() {
        let session_factory = |_, unused| {
            async move {
                tokio::time::sleep(Duration::from_millis(5000)).await;
                drop(unused); // closes
                Ok(())
            }
            .boxed()
        };

        assert_beaviour_test_succeeds(session_factory, wait_for_disconnect::<TestError>).await;
    }
}
