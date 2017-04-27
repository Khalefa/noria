use petgraph;

use backlog;
use checktable;
use flow::domain;
use flow::node;
use flow::statistics;
use flow::prelude::*;

use std::fmt;
use std::sync::mpsc;
use std::collections::HashMap;

use serde::{Serialize, Serializer, Deserialize, Deserializer};

use std::time;

use channel;

#[derive(Clone, Serialize, Deserialize)]
pub struct Link {
    pub src: NodeAddress,
    pub dst: NodeAddress,
}

impl Link {
    pub fn new(src: NodeAddress, dst: NodeAddress) -> Self {
        Link { src: src, dst: dst }
    }
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?} -> {:?}", self.src, self.dst)
    }
}

#[derive(Serialize, Deserialize)]
pub enum TriggerEndpoint {
    None,
    Start(Vec<usize>),
    End(channel::Sender<Packet>),
    Local(Vec<usize>),
}

#[derive(Serialize, Deserialize)]
enum InitialStateDef {
    PartialLocal(usize),
    IndexedLocal(Vec<Vec<usize>>),
    PartialGlobal,
    Global,
}
pub enum InitialState {
    PartialLocal(usize),
    IndexedLocal(Vec<Vec<usize>>),
    PartialGlobal(backlog::WriteHandle, backlog::ReadHandle),
    Global,
}
impl Serialize for InitialState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer
    {
        let def = match *self {
            InitialState::PartialLocal(u) => InitialStateDef::PartialLocal(u),
            InitialState::IndexedLocal(ref v) => InitialStateDef::IndexedLocal(v.clone()),
            InitialState::PartialGlobal(..) => unimplemented!(),
            InitialState::Global => InitialStateDef::Global,
        };
        def.serialize(serializer)
    }
}
impl Deserialize for InitialState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: Deserializer
    {
        let def = try!(InitialStateDef::deserialize(deserializer));
        match def {
            InitialStateDef::PartialLocal(u) => Ok(InitialState::PartialLocal(u)),
            InitialStateDef::IndexedLocal(v) => Ok(InitialState::IndexedLocal(v)),
            InitialStateDef::PartialGlobal => unimplemented!(),
            InitialStateDef::Global => Ok(InitialState::Global),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ReplayPieceContext {
    Partial {
        for_key: Vec<DataType>,
        ignore: bool,
    },
    Regular { last: bool },
}

#[derive(Clone, Serialize, Deserialize)]
pub enum TransactionState {
    Committed(i64, petgraph::graph::NodeIndex, Option<HashMap<domain::Index, i64>>),
    Pending(checktable::Token, channel::Sender<Result<i64, ()>>),
    WillCommit,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ReplayTransactionState {
    pub ts: i64,
    pub prevs: Option<HashMap<domain::Index, i64>>,
}

/// Different events that can occur as a packet is being processed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PacketEvent {
    /// The packet has been pulled off the input channel.
    ExitInputChannel,
    /// The packet has been received by some domain, and is being handled.
    Handle,
    /// The packet is being processed at some node.
    Process,
    /// The packet has reached some reader node.
    ReachedReader,
}

pub type TimeInstant = u64;
pub type Tracer = Option<channel::Sender<(TimeInstant, PacketEvent)>>;

#[derive(Serialize, Deserialize)]
pub enum Packet {
    // Data messages
    //
    /// Regular data-flow update.
    Message {
        link: Link,
        data: Records,
        tracer: Tracer,
    },

    /// Transactional data-flow update.
    Transaction {
        link: Link,
        data: Records,
        state: TransactionState,
        tracer: Tracer,
    },

    /// Update that is part of a tagged data-flow replay path.
    FullReplay { link: Link, tag: Tag, state: State },

    /// Update that is part of a tagged data-flow replay path.
    ReplayPiece {
        link: Link,
        tag: Tag,
        data: Records,
        context: ReplayPieceContext,
        transaction_state: Option<ReplayTransactionState>,
    },

    //
    // Internal control
    //
    Finish(Tag, LocalNodeIndex),

    // Control messages
    //
    /// Add a new node to this domain below the given parents.
    AddNode {
        node: domain::NodeDescriptor,
        parents: Vec<LocalNodeIndex>,
    },

    /// Add a new column to an existing `Base` node.
    AddBaseColumn {
        node: LocalNodeIndex,
        field: String,
        default: DataType,
        ack: channel::SyncSender<()>,
    },

    /// Drops an existing column from a `Base` node.
    DropBaseColumn {
        node: LocalNodeIndex,
        column: usize,
        ack: channel::SyncSender<()>,
    },

    /// Update Egress node.
    UpdateEgress {
        node: LocalNodeIndex,
        new_tx: Option<(NodeAddress, NodeAddress, channel::SyncSender<Packet>)>,
        new_tag: Option<(Tag, NodeAddress)>,
    },

    /// Add a streamer to an existing reader node.
    AddStreamer {
        node: LocalNodeIndex,
        new_streamer: channel::Sender<Vec<node::StreamUpdate>>,
    },

    /// Request a handle to an unbounded channel to this domain.
    ///
    /// We need these channels to send replay requests, as using the bounded channels could easily
    /// result in a deadlock. Since the unbounded channel is only used for requests as a result of
    /// processing, it is essentially self-clocking.
    RequestUnboundedTx(channel::Sender<channel::Sender<Packet>>),

    /// Set up a fresh, empty state for a node, indexed by a particular column.
    ///
    /// This is done in preparation of a subsequent state replay.
    PrepareState {
        node: LocalNodeIndex,
        state: InitialState,
    },

    /// Probe for the number of records in the given node's state
    StateSizeProbe {
        node: LocalNodeIndex,
        ack: channel::SyncSender<usize>,
    },

    /// Inform domain about a new replay path.
    SetupReplayPath {
        tag: Tag,
        source: Option<NodeAddress>,
        path: Vec<(NodeAddress, Option<usize>)>,
        done_tx: Option<channel::SyncSender<()>>,
        trigger: TriggerEndpoint,
        ack: channel::SyncSender<()>,
    },

    /// Ask domain (nicely) to replay a particular key.
    RequestPartialReplay { tag: Tag, key: Vec<DataType> },

    /// Instruct domain to replay the state of a particular node along an existing replay path.
    StartReplay {
        tag: Tag,
        from: NodeAddress,
        ack: channel::SyncSender<()>,
    },

    /// Sent to instruct a domain that a particular node should be considered ready to process
    /// updates.
    Ready {
        node: LocalNodeIndex,
        index: Vec<Vec<usize>>,
        ack: channel::SyncSender<()>,
    },

    /// Notification from Blender for domain to terminate
    Quit,

    // Transaction time messages
    //
    /// Instruct domain to flush pending transactions and notify upon completion. `prev_ts` is the
    /// timestamp of the last transaction sent to the domain prior to at.
    ///
    /// This allows a migration to ensure all transactions happen strictly *before* or *after* a
    /// migration in timestamp order.
    StartMigration {
        at: i64,
        prev_ts: i64,
        ack: channel::SyncSender<()>,
    },

    /// Notify a domain about a completion timestamp for an ongoing migration.
    ///
    /// Once this message is received, the domain may continue processing transactions with
    /// timestamps following the given one.
    ///
    /// The update also includes the new ingress_from_base counts the domain should use going
    /// forward.
    CompleteMigration {
        at: i64,
        ingress_from_base: HashMap<petgraph::graph::NodeIndex, usize>,
    },

    /// Request that a domain send usage statistics on the given sender.
    GetStatistics(channel::SyncSender<(statistics::DomainStats,
                                        HashMap<petgraph::graph::NodeIndex,
                                                statistics::NodeStats>)>),

    /// The packet was captured awaiting the receipt of other replays.
    Captured,

    None,
}

impl Packet {
    pub fn link(&self) -> &Link {
        match *self {
            Packet::Message { ref link, .. } => link,
            Packet::Transaction { ref link, .. } => link,
            Packet::FullReplay { ref link, .. } => link,
            Packet::ReplayPiece { ref link, .. } => link,
            _ => unreachable!(),
        }
    }

    pub fn link_mut(&mut self) -> &mut Link {
        match *self {
            Packet::Message { ref mut link, .. } => link,
            Packet::Transaction { ref mut link, .. } => link,
            Packet::FullReplay { ref mut link, .. } => link,
            Packet::ReplayPiece { ref mut link, .. } => link,
            _ => unreachable!(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match *self {
            Packet::Message { ref data, .. } => data.is_empty(),
            Packet::Transaction { ref data, .. } => data.is_empty(),
            Packet::FullReplay { .. } => false,
            Packet::ReplayPiece { ref data, .. } => data.is_empty(),
            Packet::None => true,
            _ => unreachable!(),
        }
    }

    pub fn map_data<F>(&mut self, map: F)
        where F: FnOnce(&mut Records)
    {
        match *self {
            Packet::Message { ref mut data, .. } |
            Packet::Transaction { ref mut data, .. } |
            Packet::ReplayPiece { ref mut data, .. } => {
                map(data);
            }
            _ => {
                unreachable!();
            }
        }
    }

    pub fn is_regular(&self) -> bool {
        match *self {
            Packet::Message { .. } => true,
            Packet::Transaction { .. } => true,
            _ => false,
        }
    }

    pub fn tag(&self) -> Option<Tag> {
        match *self {
            Packet::FullReplay { tag, .. } => Some(tag),
            Packet::ReplayPiece { tag, .. } => Some(tag),
            _ => None,
        }
    }

    pub fn data(&self) -> &Records {
        match *self {
            Packet::Message { ref data, .. } => data,
            Packet::Transaction { ref data, .. } => data,
            Packet::ReplayPiece { ref data, .. } => data,
            _ => unreachable!(),
        }
    }

    pub fn take_data(&mut self) -> Records {
        use std::mem;
        match mem::replace(self, Packet::None) {
            Packet::Message { data, .. } => data,
            Packet::Transaction { data, .. } => data,
            Packet::ReplayPiece { data, .. } => data,
            _ => unreachable!(),
        }
    }

    pub fn clone_data(&self) -> Self {
        match *self {
            Packet::Message {
                ref link,
                ref data,
                ref tracer,
            } => {
                Packet::Message {
                    link: link.clone(),
                    data: data.clone(),
                    tracer: tracer.clone(),
                }
            }
            Packet::Transaction {
                ref link,
                ref data,
                ref state,
                ref tracer,
            } => {
                Packet::Transaction {
                    link: link.clone(),
                    data: data.clone(),
                    state: state.clone(),
                    tracer: tracer.clone(),
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn trace(&self, event: PacketEvent) {
        match *self {
            Packet::Message { tracer: Some(ref sender), .. } |
            Packet::Transaction { tracer: Some(ref sender), .. } => {
                let _ = sender.send((0, event));
            }
            _ => {}
        }
    }

    pub fn tracer(&mut self) -> Option<&mut Tracer> {
        match *self {
            Packet::Message { ref mut tracer, .. } |
            Packet::Transaction { ref mut tracer, .. } => Some(tracer),
            _ => None,
        }
    }
}

impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Packet::Message { ref link, .. } => write!(f, "Packet::Message({:?})", link),
            Packet::Transaction {
                ref link,
                ref state,
                ..
            } => {
                match *state {
                    TransactionState::Committed(ts, ..) => {
                        write!(f, "Packet::Transaction({:?}, {})", link, ts)
                    }
                    TransactionState::Pending(..) => {
                        write!(f, "Packet::Transaction({:?}, pending)", link)
                    }
                    TransactionState::WillCommit => write!(f, "Packet::Transaction({:?}, ?)", link),
                }
            }
            Packet::ReplayPiece {
                ref link,
                ref tag,
                ref data,
                ..
            } => {
                write!(f,
                       "Packet::ReplayPiece({:?}, {}, {} records)",
                       link,
                       tag.id(),
                       data.len())
            }
            Packet::FullReplay {
                ref link,
                ref tag,
                ref state,
            } => {
                write!(f,
                       "Packet::FullReplay({:?}, {}, {} row state)",
                       link,
                       tag.id(),
                       state.len())
            }
            Packet::None => write!(f, "Packet::Node"),
            _ => write!(f, "Packet::Control"),
        }
    }
}
