use std::{convert::TryInto, pin::Pin, time::Duration};

use futures::{
    future::{join, ready},
    Future, FutureExt,
};
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
fn into_behaviour<E, C>(closure: C) -> impl BehaviourFunction<E>
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

pub fn send<T: TryInto<Vec<u8>> + Send + 'static, E: ErrorType + From<T::Error>>(
    data: T,
) -> impl BehaviourFunction<E> {
    into_behaviour(move |in_sender: &mut InSender<E>, _: &mut OutReceiver| {
        send_data(in_sender, data);
        yield_now().boxed()
    })
}

pub fn send_data<T: TryInto<Vec<u8>>, E: ErrorType + From<T::Error>>(
    in_sender: &InSender<E>,
    data: T,
) {
    in_sender
        .send(data.try_into().map_err(|e2| e2.into()))
        .expect("Connect failed");
}

pub async fn test_expectations<
    E: ErrorType,
    F: SessionFactory<E>,
    T: BehaviourFunction<E> + 'static,
>(
    session_factory: F,
    client_behaviour: T,
) {
    let (in_sender, in_receiver) = unbounded_channel::<Result<Vec<u8>, E>>();
    let (out_sender, out_receiver) = unbounded_channel();

    let session_future = session_factory(in_receiver, out_sender);

    let other_future = tokio::task::spawn(async move {
        let mut out_receiver = out_receiver;
        let mut in_sender = in_sender;

        let result = client_behaviour(&mut in_sender, &mut out_receiver).await;
        drop(result);
        out_receiver.close();
        drop(in_sender);

        ()
    });

    let results = join(session_future, other_future).await;

    assert!(results.0.is_ok());
    assert!(results.1.is_ok());
}

pub fn assert_receive<T: FnOnce(Vec<u8>) -> bool>(
    out_receiver: &mut OutReceiver,
    message_matcher: T,
) {
    let response = out_receiver.recv().now_or_never();

    if let Some(Some(bytes)) = response {
        assert!(message_matcher(bytes))
    } else {
        if response.is_none() {
            panic!("No server message");
        } else {
            panic!("Unexpected server message:{:?}", response.unwrap());
        }
    }
}

pub fn receive<E: ErrorType, T: FnOnce(Vec<u8>) -> bool + Send + 'static>(
    message_matcher: T,
) -> impl BehaviourFunction<E> {
    into_behaviour(|_: &mut InSender<E>, out_receiver: &mut OutReceiver| {
        ready(assert_receive(out_receiver, message_matcher)).boxed()
    })
}

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

    use std::convert::Infallible;

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
    impl TryInto<Vec<u8>> for TestData {
        type Error = TestError2;

        fn try_into(self) -> Result<Vec<u8>, Self::Error> {
            Err(TestError2)
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
}