use super::*;
use core::time::Duration;
use enr::NodeId;
use futures::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::time::{sleep, Instant, Sleep};
use tracing::debug;

pub const MAX_ADS_PER_TOPIC: usize = 100;
pub const MAX_ADS: i32 = 5000;
pub const AD_LIFETIME: Duration = Duration::from_secs(60 * 15);

type Topic = [u8; 32];
pub struct Ads {
    expirations: VecDeque<(Pin<Box<Sleep>>, Topic)>,
    ads: HashMap<Topic, VecDeque<(NodeId, Instant)>>,
    total_ads: i32,
}

impl Ads {
    pub fn new() -> Self {
        Ads {
            expirations: VecDeque::new(),
            ads: HashMap::new(),
            total_ads: 0,
        }
    }

    pub fn ticket_wait_time(&self, topic: Topic) -> Duration {
        let now = Instant::now();
        match self.ads.get(&topic) {
            Some(nodes) => {
                if nodes.len() < MAX_ADS_PER_TOPIC {
                    Duration::from_secs(0)
                } else {
                    match nodes.get(0) {
                        Some((_, insert_time)) => {
                            let elapsed_time = now.saturating_duration_since(*insert_time);
                            AD_LIFETIME.saturating_sub(elapsed_time)
                        }
                        None => {
                            #[cfg(debug_assertions)]
                            panic!("Panic on debug, topic was not removed when empty");
                            #[cfg(not(debug_assertions))]
                            {
                                error!("Topic was not removed when empty");
                                return Poll::Ready(Err("No nodes for topic".into()));
                            }
                        }
                    }
                }
            }
            None => {
                if self.total_ads < MAX_ADS {
                    Duration::from_secs(0)
                } else {
                    match self.expirations.get(0) {
                        Some((fut, _)) => fut.deadline().saturating_duration_since(now),
                        None => {
                            #[cfg(debug_assertions)]
                            panic!("Panic on debug, mismatched mapping between expiration queue and total ads count");
                            #[cfg(not(debug_assertions))]
                            {
                                error!("Mismatched mapping between expiration queue and total ads count");
                                return Duration::from_secs(0);
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn insert(&mut self, node_id: NodeId, topic: Topic) {
        let now = Instant::now();
        if let Some(nodes) = self.ads.get_mut(&topic) {
            nodes.push_back((node_id, now));
        } else {
            let mut nodes = VecDeque::new();
            nodes.push_back((node_id, now));
            self.ads.insert(topic, nodes);
        }
        self.expirations
            .push_back((Box::pin(sleep(Duration::from_secs(60 * 15))), topic));
        self.total_ads += 1;
    }

    // Should first be be called after checking if list is empty in 
    fn next_to_expire(&mut self) -> Result<(&mut Pin<Box<Sleep>>, Topic), String> {
        if self.expirations.is_empty() {
            return Err("No ads in 'table'".into());
        }
        match self.expirations.get_mut(0) {
            Some((fut, topic)) => Ok((fut, *topic)),
            None => {
                #[cfg(debug_assertions)]
                panic!(
                    "Panic on debug, mismatched mapping between expiration queue and entry queue"
                );
                #[cfg(not(debug_assertions))]
                {
                    error!("Mismatched mapping between expiration queue and entry queue");
                    return Err("Topic doesn't exist".into());
                }
            }
        }
    }
}

impl Stream for Ads {
    // type returned can be unit type but for testing easier to get values, worth the overhead to keep?
    type Item = Result<((NodeId, Instant), Topic), String>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.next_to_expire() {
            Ok((fut, topic)) => {
                match fut.poll_unpin(cx) {
                    Poll::Ready(()) => match self.ads.get_mut(&topic) {
                        Some(topic_ads) => match topic_ads.pop_front() {
                            Some((node_id, insert_time)) => {
                                if topic_ads.is_empty() {
                                    self.ads.remove(&topic);
                                }
                                self.total_ads -= 1;
                                Poll::Ready(Some(Ok(((node_id, insert_time), topic))))
                            }
                            None => {
                                #[cfg(debug_assertions)]
                                panic!("Panic on debug, mismatched mapping between expiration queue and entry queue");
                                #[cfg(not(debug_assertions))]
                                {
                                    error!("Mismatched mapping between expiration queue and entry queue");
                                    return Poll::Ready(Err("No nodes for topic".into()));
                                }
                            }
                        },
                        None => {
                            #[cfg(debug_assertions)]
                            panic!("Panic on debug, mismatched mapping between expiration queue and entry queue");
                            #[cfg(not(debug_assertions))]
                            {
                                error!("Mismatched mapping between expiration queue and entry queue");
                                return Poll::Ready(Err("Topic doesn't exist".into()));
                            }
                        }
                    },
                    Poll::Pending => Poll::Pending,
                }
            },
            Err(e)=> {
                debug!("{}", e);
                Poll::Pending
            }
        }
    }
}
