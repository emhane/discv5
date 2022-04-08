use super::*;
use crate::Enr;
use core::time::Duration;
use futures::prelude::*;
use more_asserts::debug_unreachable;
use std::{
    cmp::Ordering,
    collections::{vec_deque::Iter, HashMap, VecDeque},
    pin::Pin,
    task::{Context, Poll},
};
use ticket::Ticket;
use tokio::time::Instant;
use tracing::{debug, error};

mod test;
pub mod ticket;

pub type Topic = [u8; 32];

/// An ad we are adevrtising for another node
#[derive(Debug)]
pub struct AdNode {
    node_record: Enr,
    insert_time: Instant,
}

impl AdNode {
    pub fn new(node_record: Enr, insert_time: Instant) -> Self {
        AdNode {
            node_record,
            insert_time,
        }
    }

    pub fn node_record(&self) -> &Enr {
        &self.node_record
    }
}

impl PartialEq for AdNode {
    fn eq(&self, other: &Self) -> bool {
        self.node_record == other.node_record
    }
}

#[derive(Ord, Eq)]
struct AdTopic {
    topic: Topic,
    insert_time: Instant,
}

impl AdTopic {
    pub fn new(topic: Topic, insert_time: Instant) -> Self {
        AdTopic { topic, insert_time }
    }
}

impl PartialEq for AdTopic {
    fn eq(&self, other: &Self) -> bool {
        self.topic == other.topic
    }
}

impl PartialOrd for AdTopic {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.topic.cmp(&other.topic))
    }
}

pub struct Ads {
    expirations: VecDeque<AdTopic>,
    ads: HashMap<Topic, VecDeque<AdNode>>,
    total_ads: usize,
    ad_lifetime: Duration,
    max_ads_per_topic: usize,
    max_ads: usize,
}

impl Ads {
    pub fn new(
        ad_lifetime: Duration,
        max_ads_per_topic: usize,
        max_ads: usize,
    ) -> Result<Self, &'static str> {
        let (max_ads_per_topic, max_ads) = if max_ads_per_topic <= max_ads {
            (max_ads_per_topic, max_ads)
        } else {
            return Err("Values passed to max_ads_per_topic and max_ads don't make sense");
        };

        Ok(Ads {
            expirations: VecDeque::new(),
            ads: HashMap::new(),
            total_ads: 0,
            ad_lifetime,
            max_ads_per_topic,
            max_ads,
        })
    }

    pub fn get_ad_nodes(&self, topic: Topic) -> Result<Iter<'_, AdNode>, &str> {
        match self.ads.get(&topic) {
            Some(topic_ads) => Ok(topic_ads.into_iter()),
            None => Err("No ads for this topic"),
        }
    }

    pub fn ticket_wait_time(&mut self, topic: Topic) -> Option<Duration> {
        self.remove_expired();
        let now = Instant::now();
        if self.total_ads < self.max_ads {
            match self.ads.get(&topic) {
                Some(nodes) => {
                    if nodes.len() < self.max_ads_per_topic {
                        None
                    } else {
                        match nodes.get(0) {
                            Some(ad) => {
                                let elapsed_time = now.saturating_duration_since(ad.insert_time);
                                Some(self.ad_lifetime.saturating_sub(elapsed_time))
                            }
                            None => {
                                debug_unreachable!("Panic on debug,topic key should be deleted if no ad nodes queued for it");
                                error!("Topic key should be deleted if no ad nodes queued for it");
                                return None;
                            }
                        }
                    }
                }
                None => None,
            }
        } else {
            match self.expirations.get(0) {
                Some(ad) => {
                    let elapsed_time = now.saturating_duration_since(ad.insert_time);
                    Some(self.ad_lifetime.saturating_sub(elapsed_time))
                }
                None => {
                    debug_unreachable!("Panic on debug, mismatched mapping between expiration queue and total ads count");
                    error!("Mismatched mapping between expiration queue and total ads count");
                    return None;
                }
            }
        }
    }

    pub fn remove_expired(&mut self) -> Option<((Enr, Instant), Topic)> {
        let mut map: HashMap<Topic, usize> = HashMap::new();

        self.expirations
            .iter()
            .take_while(|ad| ad.insert_time.elapsed() >= self.ad_lifetime)
            .for_each(|ad| {
                let count = map.entry(ad.topic).or_default();
                *count += 1;
            });

        map.into_iter().for_each(|(topic, index)| {
            let entry_ref = self.ads.entry(topic).or_default();
            for _ in 0..index {
                entry_ref.pop_front();
            }
            if entry_ref.is_empty() {
                self.ads.remove(&topic);
            }
            self.total_ads -= index;
        });

        None
    }

    pub fn regconfirmation(
        &mut self,
        node_record: Enr,
        topic: Topic,
        wait_time: Duration,
        _ticket: Ticket,
    ) -> Result<(), &str> {
        if wait_time > Duration::from_secs(0) {
            return Err("currently no space for this ad");
        }
        // do some validation of tiket against other tickets received in registration window
        self.insert(node_record, topic)
    }

    fn insert(&mut self, node_record: Enr, topic: Topic) -> Result<(), &str> {
        self.remove_expired();
        let now = Instant::now();
        let nodes = self.ads.entry(topic).or_default();
        if nodes.contains(&AdNode::new(node_record.clone(), now)) {
            error!(
                "This node {} is already advertising this topic",
                node_record.node_id()
            );
            return Err("Node already advertising this topic");
        }
        nodes.push_back(AdNode {
            node_record,
            insert_time: now,
        });
        self.expirations.push_back(AdTopic::new(topic, now));
        self.total_ads += 1;
        Ok(())
    }
}
