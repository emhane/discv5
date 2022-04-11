use super::*;
use crate::{
    enr::{CombinedKey, EnrBuilder},
    rpc::RequestId,
};
use delay_map::HashMapDelay;
use enr::NodeId;
use node_info::NodeContact;
use std::{cmp::Eq, net::IpAddr};

// Placeholder function
pub fn topic_hash(topic: Vec<u8>) -> Topic {
    let mut topic_hash = [0u8; 32];
    topic_hash[32 - topic.len()..].copy_from_slice(&topic);
    topic_hash
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub struct ActiveTopic {
    node_id: NodeId,
    topic: Topic,
}

impl ActiveTopic {
    pub fn new(node_id: NodeId, topic: Topic) -> Self {
        ActiveTopic { node_id, topic }
    }

    pub fn topic(&self) -> Topic {
        self.topic
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Ticket {
    //nonce: u64,
    src_node_id: NodeId,
    src_ip: IpAddr,
    topic: Topic,
    req_time: Instant,
    wait_time: Duration,
    //cum_wait: Option<Duration>,*/
}

// DEBUG
impl Default for Ticket {
    fn default() -> Self {
        let port = 5000;
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        let key = CombinedKey::generate_secp256k1();

        let enr = EnrBuilder::new("v4").ip(ip).udp(port).build(&key).unwrap();
        let node_id = enr.node_id();

        Ticket {
            src_node_id: node_id,
            src_ip: ip,
            topic: [0u8; 32],
            req_time: Instant::now(),
            wait_time: Duration::default(),
        }
    }
}

impl PartialEq for Ticket {
    fn eq(&self, other: &Self) -> bool {
        self.src_node_id == other.src_node_id
            && self.src_ip == other.src_ip
            && self.topic == other.topic
    }
}

impl Ticket {
    pub fn new(
        //nonce: u64,
        src_node_id: NodeId,
        src_ip: IpAddr,
        topic: Topic,
        req_time: Instant,
        wait_time: Duration,
    ) -> Self {
        Ticket {
            //nonce,
            src_node_id,
            src_ip,
            topic,
            req_time,
            wait_time,
        }
    }

    pub fn decode(_ticket_bytes: Vec<u8>) -> Result<Self, String> {
        Ok(Ticket::default())
    }
}

pub struct ActiveTicket {
    contact: NodeContact,
    ticket: Ticket,
}

impl ActiveTicket {
    pub fn new(contact: NodeContact, ticket: Ticket) -> Self {
        ActiveTicket { contact, ticket }
    }

    pub fn contact(&self) -> NodeContact {
        self.contact.clone()
    }

    pub fn ticket(&self) -> Ticket {
        self.ticket
    }
}

/// Tickets received from other nodes as response to REGTOPIC req
pub struct Tickets {
    tickets: HashMapDelay<ActiveTopic, ActiveTicket>,
    ticket_history: TicketHistory,
}

impl Tickets {
    pub fn new(ticket_cache_duration: Duration) -> Self {
        Tickets {
            tickets: HashMapDelay::new(Duration::default()),
            ticket_history: TicketHistory::new(ticket_cache_duration),
        }
    }

    pub fn insert(
        &mut self,
        contact: NodeContact,
        ticket: Ticket,
        wait_time: Duration,
    ) -> Result<(), &str> {
        let active_topic = ActiveTopic::new(contact.node_id(), ticket.topic);

        if let Err(e) = self.ticket_history.insert(active_topic) {
            return Err(e);
        }
        self.tickets
            .insert_at(active_topic, ActiveTicket::new(contact, ticket), wait_time);
        Ok(())
    }
}

impl Stream for Tickets {
    type Item = Result<(ActiveTopic, ActiveTicket), String>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.tickets.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok((active_topic, ticket)))) => {
                Poll::Ready(Some(Ok((active_topic, ticket))))
            }
            Poll::Ready(Some(Err(e))) => {
                debug!("{}", e);
                Poll::Pending
            }
            Poll::Ready(None) => Poll::Pending,
            Poll::Pending => Poll::Pending,
        }
    }
}

struct TicketRateLimiter {
    active_topic: ActiveTopic,
    first_seen: Instant,
}

#[derive(Default)]
struct TicketHistory {
    ticket_cache: HashMap<ActiveTopic, u8>,
    expirations: VecDeque<TicketRateLimiter>,
    ticket_cache_duration: Duration,
}

impl TicketHistory {
    fn new(ticket_cache_duration: Duration) -> Self {
        TicketHistory {
            ticket_cache: HashMap::new(),
            expirations: VecDeque::new(),
            ticket_cache_duration,
        }
    }

    pub fn insert(&mut self, active_topic: ActiveTopic) -> Result<(), &str> {
        self.remove_expired();
        let count = self.ticket_cache.entry(active_topic).or_default();
        if *count >= 3 {
            error!("Max 3 tickets per (NodeId, Topic) accepted in 15 minutes");
            return Err("Ticket limit reached");
        }
        *count += 1;
        Ok(())
    }

    fn remove_expired(&mut self) {
        let now = Instant::now();
        let cached_tickets = self
            .expirations
            .iter()
            .take_while(|ticket_limiter| {
                now.saturating_duration_since(ticket_limiter.first_seen)
                    >= self.ticket_cache_duration
            })
            .map(|ticket_limiter| ticket_limiter.active_topic)
            .collect::<Vec<ActiveTopic>>();

        cached_tickets.iter().for_each(|active_topic| {
            self.ticket_cache.remove(active_topic);
            self.expirations.pop_front();
        });
    }
}

#[derive(Clone, Copy)]
struct RegistrationWindow {
    topic: Topic,
    open_time: Instant,
}

pub struct TicketPools {
    ticket_pools: HashMap<Topic, HashMap<NodeId, (Enr, RequestId, Ticket)>>,
    expirations: VecDeque<RegistrationWindow>,
}

impl TicketPools {
    pub fn new() -> Self {
        TicketPools {
            ticket_pools: HashMap::new(),
            expirations: VecDeque::new(),
        }
    }

    pub fn insert(&mut self, node_record: Enr, req_id: RequestId, ticket: Ticket) {
        let open_time = ticket.req_time.checked_add(ticket.wait_time).unwrap();
        if open_time.elapsed() > Duration::from_secs(10) {
            return;
        }
        let pool = self.ticket_pools.entry(ticket.topic).or_default();
        if pool.is_empty() {
            self.expirations.push_back(RegistrationWindow {
                topic: ticket.topic,
                open_time,
            });
        }
        pool.insert(node_record.node_id(), (node_record, req_id, ticket));
    }
}

impl Stream for TicketPools {
    type Item = Result<(Topic, Enr, RequestId), String>;
    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.expirations
            .get(0)
            .map(|reg_window| *reg_window)
            .map(|reg_window| {
                if reg_window.open_time.elapsed() >= Duration::from_secs(10) {
                    self.ticket_pools
                        .remove_entry(&reg_window.topic)
                        .map(|(topic, ticket_pool)| {
                            // do some proper selection based on node_address and ticket
                            let (_node_id, (node_record, req_id, _ticket)) =
                                ticket_pool.into_iter().next().unwrap();
                            self.expirations.pop_front();
                            Poll::Ready(Some(Ok((topic, node_record, req_id))))
                        })
                        .unwrap_or(Poll::Ready(Some(Err("Ticket selection failed".into()))))
                } else {
                    Poll::Pending
                }
            })
            .unwrap_or(Poll::Pending)
    }
}
