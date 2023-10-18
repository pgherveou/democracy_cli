#[subxt::subxt(runtime_metadata_path = "metadata.scale")]
pub mod kitchensink {}

use std::fmt::Display;

use crate::kitchensink::runtime_types::frame_system::AccountInfo;
use crate::kitchensink::runtime_types::{
    frame_support::traits::preimages::Bounded, pallet_democracy::vote::AccountVote,
    pallet_democracy::vote::Vote,
};
use anyhow::Result;
use clap::Parser;
use codec::Encode;
use subxt::blocks::ExtrinsicEvents;
use subxt::config::Hasher;
use subxt::events::StaticEvent;
use subxt::ext::futures::{StreamExt, TryStreamExt};
use subxt::tx::TxPayload;
use subxt::utils::H256;
use subxt::{config::substrate::BlakeTwo256, *};
use subxt_signer::sr25519::dev;

// Parsed command instructions from the command line
#[derive(Parser)]
#[clap(author, about, version)]
struct CliCommand {
    #[clap(long, default_value = "ws://127.0.0.1:9944")]
    url: String,

    #[clap(short, long, default_value = "alice")]
    user: User,

    #[clap(subcommand)]
    command: SubCommand,
}

// Dev users supported by the program
#[derive(PartialEq, Debug, Clone, Copy)]
enum User {
    Alice,
    Bob,
}
impl Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!("{:?}", self))
    }
}
impl User {
    fn keypair(&self) -> subxt_signer::sr25519::Keypair {
        match self {
            Self::Alice => dev::alice(),
            Self::Bob => dev::bob(),
        }
    }
}
impl From<&str> for User {
    fn from(s: &str) -> Self {
        match s {
            "alice" => Self::Alice,
            "bob" => Self::Bob,
            _ => panic!("invalid user"),
        }
    }
}

/// The subcommand to execute
#[derive(Parser, Debug)]
enum SubCommand {
    ShowBalance,
    CreateRemarkPreimage {
        remark: String,
    },
    MakeProposal {
        hash: String,
        len: u32,
    },
    Vote {
        index: u32,
        balance: u128,
        conviction: u8,
    },
    TrackProposalStatus,
}

// Create a vote for a proposal
fn create_vote(
    ref_index: u32,
    aye: bool,
    conviction: u8,
    balance: u128,
) -> subxt::tx::Payload<kitchensink::democracy::calls::types::Vote> {
    let vote = conviction | if aye { 0b1000_0000 } else { 0 };
    let democracy = kitchensink::tx().democracy();
    let vote = AccountVote::Standard {
        vote: Vote(vote), // Aye + Convinction::None
        balance,
    };

    democracy.vote(ref_index, vote)
}

// The program context
struct Program {
    api: OnlineClient<SubstrateConfig>,
    user: User,
}

// Helper macro to print to the console using the program context
macro_rules! print {
    ($prg:expr, $($arg:tt)*) => {
        println!("[{}] {}", $prg.user, format!($($arg)*));
    };
}

impl Program {
    /// Create a new program context
    async fn new(url: &str, user: User) -> Result<Self> {
        let api = OnlineClient::<SubstrateConfig>::from_url(url).await?;
        Ok(Self { api, user })
    }

    /// Wait for a specific event to occur
    async fn wait_for_event<Ev: StaticEvent>(&self) -> Result<Ev> {
        let event = self
            .api
            .blocks()
            .subscribe_finalized()
            .await?
            .try_filter_map(|block| async move { block.events().await?.find_first::<Ev>() })
            .boxed()
            .try_next()
            .await?;

        event.ok_or_else(|| anyhow::anyhow!("event not found"))
    }

    /// Submit a transaction and wait for it to be finalized
    async fn submit_and_watch(
        &self,
        tx: &impl TxPayload,
    ) -> Result<ExtrinsicEvents<SubstrateConfig>, subxt::Error> {
        self.api
            .tx()
            .sign_and_submit_then_watch_default(tx, &self.user.keypair())
            .await
            .map(|e| {
                print!(self, "waiting for transaction to be in block...");
                e
            })?
            .wait_for_finalized_success()
            .await
    }
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let CliCommand { url, command, user } = CliCommand::parse();
    let program = Program::new(&url, user).await?;

    match command {
        SubCommand::ShowBalance => {
            let account = user.keypair().public_key().into();
            let api = program.api.storage().at_latest().await?;

            let query = kitchensink::storage().system().account(&account);
            let AccountInfo { data, .. } = api.fetch_or_default(&query).await?;
            print!(program, "account: {data:?}");

            let query = kitchensink::storage().balances().holds(&account);
            let holds = api.fetch_or_default(&query).await?;
            print!(program, "holds: {holds:?}");

            let query = kitchensink::storage().balances().freezes(&account);
            let freezes = api.fetch_or_default(&query).await?;
            print!(program, "freezes: {freezes:?}");
        }
        SubCommand::CreateRemarkPreimage { remark } => {
            let image = kitchensink::Call::System(
                kitchensink::runtime_types::frame_system::pallet::Call::remark {
                    remark: remark.into_bytes(),
                },
            )
            .encode();
            let image_hash = BlakeTwo256::hash(&image);
            let image_len = image.len() as u32;

            print!(program, "adding image: {}", hex::encode(&image));
            let preimage = kitchensink::tx().preimage();
            let tx = preimage.note_preimage(image);
            program.submit_and_watch(&tx).await?;
            print!(program, "preimage created ({image_hash:?}, {image_len})");
        }
        SubCommand::MakeProposal { hash, len } => {
            let democracy = kitchensink::tx().democracy();
            let hash = H256::from_slice(&hex::decode(hash)?);
            let runtime_call = Bounded::Lookup { hash, len };

            print!(program, "creating proposal for ({hash}, {len})");
            let tx = democracy.propose(runtime_call, 1_000_000_000_000_000_000u128);
            let events = program.submit_and_watch(&tx).await?;
            print!(program, "proposal created {:?}", events);

            let tabled = program
                .wait_for_event::<kitchensink::democracy::events::Tabled>()
                .await;
            print!(program, "proposal tabled {:?}", tabled);

            let started = program
                .wait_for_event::<kitchensink::democracy::events::Started>()
                .await;
            print!(program, "proposal started {:?}", started);
        }
        SubCommand::Vote {
            index,
            balance,
            conviction,
        } => {
            print!(program, "submitting vote");
            let vote = create_vote(index, true, conviction, balance);
            let events = program.submit_and_watch(&vote).await?;
            let vote_event = events.find_first::<kitchensink::democracy::events::Voted>()?;
            print!(program, "vote finalized {:?}", vote_event);
        }
        SubCommand::TrackProposalStatus => {
            let passed = program
                .wait_for_event::<kitchensink::democracy::events::Passed>()
                .await;
            print!(program, "proposal passed {:?}", passed);
        }
    }

    Ok(())
}
