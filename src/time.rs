use std::{
    convert::Infallible,
    time::{Duration, Instant},
};

use futures::{stream::BoxStream, StreamExt};

use crate::{Topic, TopicManager};

pub struct Interval {
    dur: Duration,
}

impl Interval {
    pub fn new(dur: Duration) -> Self {
        Self { dur }
    }
}

impl<S> Topic<S> for Interval
where
    S: Send + Sync + 'static,
{
    type Output = Instant;

    type Error = Infallible;

    fn topic(&self) -> String {
        format!("{:?}", self.dur)
    }

    fn init(&self, _manager: &TopicManager<S>) -> BoxStream<'static, Result<Self::Output, Self::Error>> {
        let dur = self.dur;

        let stream = async_stream::stream! {
            let mut ins = Instant::now();
            loop {
                yield Ok(ins);
                ins = Instant::now();
                tokio::time::sleep(dur).await;
            }
        };

        stream.boxed()
    }
}

pub struct Timeout {
    dur: Duration,
}

impl Timeout {
    pub fn new(dur: Duration) -> Self {
        Self { dur }
    }
}

impl<S> Topic<S> for Timeout
where
    S: Send + Sync + 'static,
{
    type Output = Instant;

    type Error = Infallible;

    fn topic(&self) -> String {
        format!("{:?}", self.dur)
    }

    fn init(&self, _manager: &TopicManager<S>) -> BoxStream<'static, Result<Self::Output, Self::Error>> {
        let dur = self.dur;

        let stream = async_stream::stream! {
            let ins = Instant::now();
            tokio::time::sleep(dur).await;
            yield Ok(ins);
        };

        stream.boxed()
    }
}
