//! A libfuzzer-like fuzzer with llmp-multithreading support and restarts
//! The `launcher` will spawn new processes for each cpu core.
use ahash::AHasher;
use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use bit_vec::BitVec;
use clap::{self, StructOpt};
use core::time::Duration;
use packed_struct::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json;
use std::{
    rc::Rc,
    cell::RefCell,
    cmp::min,
    collections::{BTreeMap, HashSet},
    env,
    fs::File,
    hash::Hasher,
    io::Read,
    marker::PhantomData,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use libafl::{
    bolts::{
        current_nanos, current_time,
        fs::write_file_atomic,
        launcher::Launcher,
        os::Cores,
        ownedref::OwnedSlice,
        rands::Rand,
        rands::StdRand,
        shmem::{ShMemProvider, StdShMemProvider},
        tuples::{tuple_list, Merge, Named},
        HasLen,
    },
    corpus::{Corpus, InMemoryCorpus, OnDiskCorpus},
    events::SimpleEventManager,
    events::{EventConfig, EventFirer},
    executors::{CommandExecutor, DiffExecutor},
    feedback_and_fast,
    feedbacks::{differential::DiffResult, DiffFeedback, Feedback, FeedbackState},
    fuzzer::{Fuzzer, StdFuzzer},
    generators::Generator,
    generators::RandBytesGenerator,
    inputs::{HasBytesVec, HasTargetBytes, Input},
    monitors::MultiMonitor,
    mutators::{
        scheduled::{havoc_mutations, tokens_mutations, StdScheduledMutator},
        token_mutations::Tokens,
    },
    observers::{ObserversTuple, StdOutObserver},
    schedulers::{IndexesLenTimeMinimizerScheduler, QueueScheduler},
    stages::StdMutationalStage,
    state::{HasClientPerfMonitor, HasCorpus, HasMetadata, HasRand, StdState},
    Error,
};

/// Parses a millseconds int into a [`Duration`], used for commandline arg parsing
fn timeout_from_millis_str(time: &str) -> Result<Duration, Error> {
    Ok(Duration::from_millis(time.parse()?))
}

#[derive(Debug, StructOpt)]
#[clap(
    name = "NeoDiff LibAFL Differntial Fuzzer",
    about = "A Differential fuzzer for EVM",
    author = "Andrea Fioraldi <andreafioraldi@gmail.com>, Dominik Maier <domenukk@gmail.com>"
)]
struct Opt {
    /*#[clap(
        short,
        long,
        parse(try_from_str = Cores::from_cmdline),
        help = "Spawn a client in each of the provided cores. Broker runs in the 0th core. 'all' to select all available cores. 'none' to run a client without binding to any core. eg: '1,2-4,6' selects the cores 1,2,3,4,6.",
        name = "CORES"
    )]
    cores: Cores,

    #[clap(
        short = 'p',
        long,
        help = "Choose the broker TCP port, default is 1337",
        name = "PORT"
    )]
    broker_port: u16,

    #[clap(
        parse(try_from_str),
        short = 'a',
        long,
        help = "Specify a remote broker",
        name = "REMOTE"
    )]
    remote_broker_addr: Option<SocketAddr>,*/
    #[clap(
        parse(try_from_str),
        short,
        long,
        help = "Set an initial corpus directory",
        name = "INPUT"
    )]
    input: Vec<PathBuf>,

    #[clap(
        short,
        long,
        parse(try_from_str),
        help = "Set the output directory, default is ./out",
        name = "OUTPUT",
        default_value = "./out"
    )]
    output: PathBuf,

    #[clap(
        parse(try_from_str = timeout_from_millis_str),
        short,
        long,
        help = "Set the exeucution timeout in milliseconds, default is 1000",
        name = "TIMEOUT",
        default_value = "1000"
    )]
    timeout: Duration,

    #[clap(
        parse(from_os_str),
        short = 'x',
        long,
        help = "Feed the fuzzer with an user-specified list of tokens (often called \"dictionary\"",
        name = "TOKENS",
        multiple_occurrences = true
    )]
    tokens: Vec<PathBuf>,

    #[clap(
        short,
        long,
        help = "no fuzz",
        name = "NOFUZZ",
    )]
    nofuzz: bool,
}

/// A bytes input is the basic input
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct HexInput {
    /// The raw input bytes
    bytes: Vec<u8>,
}

static mut EXECS: usize = 0;
static mut START_TIME: Duration = Duration::from_secs(0);
static mut TYPE_HASHES: (TypeHash, TypeHash, u64, u64) = (
    TypeHash {
        mem_flag: false,
        t1: 0,
        t2: 0,
        opcode: 0,
    },
    TypeHash {
        mem_flag: false,
        t1: 0,
        t2: 0,
        opcode: 0,
    },
    0,0
);

impl Input for HexInput {
    #[cfg(feature = "std")]
    /// Write this input to the file
    fn to_file<P>(&self, path: P) -> Result<(), Error>
    where
        P: AsRef<Path>,
    {
        write_file_atomic(path, &self.bytes)
    }

    /// Load the content of this input from a file
    #[cfg(feature = "std")]
    fn from_file<P>(path: P) -> Result<Self, Error>
    where
        P: AsRef<Path>,
    {
        let mut file = File::open(path)?;
        let mut bytes: Vec<u8> = vec![];
        file.read_to_end(&mut bytes)?;
        Ok(HexInput::new(bytes))
    }

    /// Generate a name for this input
    fn generate_name(&self, _idx: usize) -> String {
        unsafe {
            format!(
                "typehash_{}-{}_pc:{}-{}_time:{}_execs:{}",
                TYPE_HASHES.0.tostr(),
                TYPE_HASHES.1.tostr(),
                TYPE_HASHES.2,
                TYPE_HASHES.3,
                (current_time() - START_TIME).as_secs(),
                EXECS
            )
        }
    }
}

impl HasBytesVec for HexInput {
    #[inline]
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[inline]
    fn bytes_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }
}

impl HasTargetBytes for HexInput {
    #[inline]
    fn target_bytes(&self) -> OwnedSlice<u8> {
        let h = hex::encode(&self.bytes).into_bytes();
        OwnedSlice::from(h)
    }
}

impl HasLen for HexInput {
    #[inline]
    fn len(&self) -> usize {
        self.bytes.len()
    }
}

impl HexInput {
    /// Creates a new bytes input using the given bytes
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

const DUMMY_BYTES_MAX: usize = 64;

#[derive(Clone, Debug)]
/// Generates random bytes
pub struct RandHexGenerator<S>
where
    S: HasRand,
{
    max_size: usize,
    phantom: PhantomData<S>,
}

impl<S> Generator<HexInput, S> for RandHexGenerator<S>
where
    S: HasRand,
{
    fn generate(&mut self, state: &mut S) -> Result<HexInput, Error> {
        let mut size = state.rand_mut().below(self.max_size as u64);
        if size == 0 {
            size = 1;
        }
        let random_bytes: Vec<u8> = (0..size)
            .map(|_| state.rand_mut().below(256) as u8)
            .collect();
        Ok(HexInput::new(random_bytes))
    }

    /// Generates up to `DUMMY_BYTES_MAX` non-random dummy bytes (0)
    fn generate_dummy(&self, _state: &mut S) -> HexInput {
        let size = min(self.max_size, DUMMY_BYTES_MAX);
        HexInput::new(vec![0; size])
    }
}

impl<S> RandHexGenerator<S>
where
    S: HasRand,
{
    /// Returns a new [`RandBytesGenerator`], generating up to `max_size` random bytes.
    #[must_use]
    pub fn new(max_size: usize) -> Self {
        Self {
            max_size,
            phantom: PhantomData,
        }
    }
}

// {"depth":1,"gas":"0x1337","gasCost":"0x0","memory":"0x","op":34,"opName":"","pc":0,"stack":[],"storage":{}}
// {"error":"EVM: Bad instruction 22","gasUsed":"0x1337","time":141}
#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct EVMLog {
    //depth: u8,
    //gas: String,
    //op_name: String,
    #[serde(default)]
    pc: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    gas_cost: Option<String>,
    #[serde(default)]
    memory: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    op: Option<u8>,
    #[serde(default)]
    stack: Option<Vec<String>>,
    #[serde(default)]
    storage: BTreeMap<String, String>,
    //#[serde(default)]
    //error: Option<String>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Hash, PartialEq, Eq, PackedStruct, Clone, Default)]
#[packed_struct(bit_numbering = "msb0")]
pub struct TypeHash {
    #[packed_field(bits = "0")]
    mem_flag: bool,
    #[packed_field(bits = "1..=4")]
    t1: u8,
    #[packed_field(bits = "5..=7")]
    t2: u8,
    #[packed_field(bits = "8..=15")]
    opcode: u8,
}

impl TypeHash {
    fn tostr(&self) -> String {
        let mut s = format!("{}_", self.opcode as usize);
        if self.t1 != 0 {
            s += &format!("{}", self.t1 as usize);
        }
        if self.t2 != 0 {
            s += &format!("{}", self.t2 as usize);
        }
        if self.mem_flag {
            s += "6";
        }
        s
    }
}

#[derive(Debug, Clone)]
pub struct EVMTypeHashFeedback {
    no_errors: bool,
    name: String,
    stdout_name: String,
}

impl Named for EVMTypeHashFeedback {
    fn name(&self) -> &str {
        &self.name
    }
}

impl EVMTypeHashFeedback {
    pub fn new(name: &str, no_errors: bool, stdout: &StdOutObserver) -> Self {
        Self {
            no_errors,
            name: name.to_string(),
            stdout_name: stdout.name().to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EVMFeedbackState {
    name: String,
    history: BitVec<u32>,
}
impl FeedbackState for EVMFeedbackState {}
impl Named for EVMFeedbackState {
    fn name(&self) -> &str {
        &self.name
    }
}

/// A `typehash`-based feedback, see [the `NeoDiff` paper](https://github.com/fgsect/NeoDiff/raw/main/roots21-2.pdf) for explanation
impl<I, S> Feedback<I, S> for EVMTypeHashFeedback
where
    I: Input,
    S: HasClientPerfMonitor,
{
    type FeedbackState = EVMFeedbackState;

    fn is_interesting<EM, OT>(
        &mut self,
        state: &mut S,
        feedback_state: &mut Self::FeedbackState,
        manager: &mut EM,
        input: &I,
        observers: &OT,
        exit_kind: &libafl::executors::ExitKind,
    ) -> Result<bool, Error>
    where
        EM: EventFirer<I>,
        OT: ObserversTuple<I, S>,
    {
        // from https://github.com/fgsect/NeoDiff/blob/ad5d1250238fcb3bc8a8ddfe0d0dcefd5703324b/EVMFuzz.py#L40

        // generate a filename
        //let mut checksum = AHasher::default();
        let mut idxs = vec![];

        let stdout_observer = observers
            .match_name::<StdOutObserver>(&self.stdout_name)
            .unwrap();
        let stdout = stdout_observer.stdout.as_ref().unwrap();
        let mut is_error = false;

        let json_log: Vec<EVMLog> = stdout
            .split('\n')
            .map(|line| serde_json::from_str(line))
            .filter(|x| x.is_ok())
            .map(|res| res.unwrap())
            .collect();

        for window in json_log.as_slice().windows(2) {
            let curr = &window[0];
            let next = &window[1];
            if curr.op.is_none() {
                continue;
            }
            let op = curr.op.unwrap();
            if op == 253 || op == 0 {
                // ignore REVERT opcode
                continue;
            }
            if curr.gas_cost.is_none() {
                continue;
            }
            // update hash

            let mut types_vec = vec![];
            let mut mem_flag = false;

            if (next.error.is_some() && next.error.as_ref().unwrap().len() > 0)
                && (next.op.is_none() || next.output.is_some())
            {
                is_error = true;
            } else if curr.error.is_some() && curr.error.as_ref().unwrap().len() > 0 {
                is_error = true;
            } else if next.stack.is_some() && next.op.is_some() && next.op.unwrap() != 0 {
                for item in next.stack.as_ref().unwrap().iter().rev() {
                    //checksum.write(item.as_bytes());
                    if types_vec.len() < 2 {
                        if u32::from_str_radix(item, 16).is_ok() {
                            types_vec.push(1);
                        } else if item.len() == 40 || item.len() == 42 {
                            types_vec.push(2);
                        } else if item.len() > 42 {
                            types_vec.push(3);
                        // ignoring elif len(item) <= 0xFFFFFFFFFFFFFFFF:
                        } else if item.len() < 40 {
                            types_vec.push(5);
                        }
                    }
                }

                let memory = curr.memory.as_ref().unwrap();
                if memory.len() > 2 {
                    mem_flag = true;
                }
            }
            // Todo: write log to somewhere?
            while types_vec.len() < 2 {
                types_vec.push(0);
            }

            let type_hash = TypeHash {
                mem_flag,
                t1: types_vec[0],
                t2: types_vec[1],
                opcode: op,
            };

            let arr = type_hash.pack().unwrap();
            let idx = (((arr[0] as u16) << 8) | arr[1] as u16) as usize;

            idxs.push((idx, type_hash));

            // eprintln!("IDX {} {}", idx, feedback_state.history.get(idx).unwrap());
        }

        if is_error && self.no_errors {
            return Ok(false);
        }

        let mut res = false;
        for (idx, type_hash) in idxs {
            if feedback_state.history.get(idx).unwrap() == false {
                res = true;
                feedback_state.history.set(idx, true);
            }
        }

        Ok(res)
    }

    fn init_state(&mut self) -> Result<Self::FeedbackState, Error> {
        Ok(EVMFeedbackState {
            name: self.name().to_string(),
            history: BitVec::from_elem(u16::MAX.into(), false),
        })
    }
}

/// The main fn, `no_mangle` as it is a C symbol
pub fn main() {
    // Registry the metadata types used in this fuzzer
    // Needed only on no_std
    //RegistryBuilder::register::<Tokens>();

    let workdir = env::current_dir().unwrap();

    let opt = Opt::parse();

    let mut diffing_hashes = HashSet::new();
    let mut opcodes = Rc::new(RefCell::new(HashSet::new()));

    let mut observers_cmp = |stdout1: &str, stdout2: &str| -> bool {
        let mut data = vec![];

        let mut is_error = false;
        let mut checksum = AHasher::default();

        /*eprintln!(
            ">>> {} {}",
            stdout_observer.name(),
            &stdout_observer.stdout.as_ref().unwrap()
        );*/
        let json_log: Vec<EVMLog> = stdout1
            .split('\n')
            //.map(|line| { let x = serde_json::from_str(line); eprintln!("{:?} {}", &x, line); x })
            .map(|line| serde_json::from_str(line))
            .filter(|x| x.is_ok())
            .map(|res| res.unwrap())
            .collect();
        //eprintln!("JSON {:?}", &json_log);

        for window in json_log.as_slice().windows(2) {
            let curr = &window[0];
            let next = &window[1];
            if curr.op.is_none() {
                continue;
            }
            let op = curr.op.unwrap();
            if op == 253 || op == 0 {
                // ignore REVERT opcode
                continue;
            }
            if curr.gas_cost.is_none() {
                continue;
            }
            let pc = curr.pc.unwrap();

            let mut types_vec = vec![];
            let mut mem_flag = false;

            if (next.error.is_some() && next.error.as_ref().unwrap().len() > 0)
                && (next.op.is_none() || next.output.is_some())
            {
                is_error = true;
            } else if curr.error.is_some() && curr.error.as_ref().unwrap().len() > 0 {
                is_error = true;
            } else if next.stack.is_some() && next.op.is_some() && next.op.unwrap() != 0 {
                for item in next.stack.as_ref().unwrap().iter().rev() {
                    checksum.write(item.as_bytes());
                    if types_vec.len() < 2 {
                        if u32::from_str_radix(item, 16).is_ok() {
                            types_vec.push(1);
                        } else if item.len() == 40 || item.len() == 42 {
                            types_vec.push(2);
                        } else if item.len() > 42 {
                            types_vec.push(3);
                        // ignoring elif len(item) <= 0xFFFFFFFFFFFFFFFF:
                        } else if item.len() < 40 {
                            types_vec.push(5);
                        }
                    }
                }
                let memory = curr.memory.as_ref().unwrap();
                checksum.write(memory.trim_end_matches('0').as_bytes());
                if memory.len() > 2 {
                    mem_flag = true;
                }

                checksum.write(&[op]);
                checksum.write(curr.gas_cost.as_ref().unwrap().as_bytes());
            } else {
                checksum.write(&[op]);
                checksum.write(curr.gas_cost.as_ref().unwrap().as_bytes());
            }

            while types_vec.len() < 2 {
                types_vec.push(0);
            }

            let type_hash = TypeHash {
                mem_flag,
                t1: types_vec[0],
                t2: types_vec[1],
                opcode: op,
            };

            data.push((checksum.finish(), type_hash, is_error, op, pc));
        }

        let mut is_error = false;
        let mut checksum = AHasher::default();

        /*eprintln!(
            ">>> {} {}",
            stdout_observer.name(),
            &stdout_observer.stdout.as_ref().unwrap()
        );*/
        let json_log: Vec<EVMLog> = stdout2
            .split('\n')
            //.map(|line| { let x = serde_json::from_str(line); eprintln!("{:?} {}", &x, line); x })
            .map(|line| serde_json::from_str(line))
            .filter(|x| x.is_ok())
            .map(|res| res.unwrap())
            .collect();
        //eprintln!("JSON {:?}", &json_log);

        let mut i = 0;
        for window in json_log.as_slice().windows(2) {
            let curr = &window[0];
            let next = &window[1];
            if curr.op.is_none() {
                continue;
            }
            let op = curr.op.unwrap();
            if op == 253 || op == 0 {
                // ignore REVERT opcode
                continue;
            }
            if curr.gas_cost.is_none() {
                continue;
            }
            let pc = curr.pc.unwrap();

            let mut types_vec = vec![];
            let mut mem_flag = false;

            if (next.error.is_some() && next.error.as_ref().unwrap().len() > 0)
                && (next.op.is_none() || next.output.is_some())
            {
                is_error = true;
            } else if curr.error.is_some() && curr.error.as_ref().unwrap().len() > 0 {
                is_error = true;
            } else if next.stack.is_some() && next.op.is_some() && next.op.unwrap() != 0 {
                for item in next.stack.as_ref().unwrap().iter().rev() {
                    checksum.write(item.as_bytes());
                    if types_vec.len() < 2 {
                        if u32::from_str_radix(item, 16).is_ok() {
                            types_vec.push(1);
                        } else if item.len() == 40 || item.len() == 42 {
                            types_vec.push(2);
                        } else if item.len() > 42 {
                            types_vec.push(3);
                        // ignoring elif len(item) <= 0xFFFFFFFFFFFFFFFF:
                        } else if item.len() < 40 {
                            types_vec.push(5);
                        }
                    }
                }
                let memory = curr.memory.as_ref().unwrap();
                checksum.write(memory.trim_end_matches('0').as_bytes());
                if memory.len() > 2 {
                    mem_flag = true;
                }

                checksum.write(&[op]);
                checksum.write(curr.gas_cost.as_ref().unwrap().as_bytes());
            } else {
                checksum.write(&[op]);
                checksum.write(curr.gas_cost.as_ref().unwrap().as_bytes());
            }

            while types_vec.len() < 2 {
                types_vec.push(0);
            }

            let type_hash = TypeHash {
                mem_flag,
                t1: types_vec[0],
                t2: types_vec[1],
                opcode: op,
            };

            if i < data.len() {
                let ck = checksum.finish();

                let (ck1, th1, err1, op1, pc1) = data[i].clone();

                if ((!err1 || !is_error) && ck1 != ck) || (err1 && !is_error) || (!err1 && is_error) {
                    let t = (th1.clone(), type_hash.clone());

                    unsafe {
                        if !diffing_hashes.contains(&t) {
                            TYPE_HASHES = (th1.clone(), type_hash.clone(), pc1, pc);
                            diffing_hashes.insert(t);

                            if op1 == op {
                                opcodes.borrow_mut().insert(op as u64);
                            }

                            return true;
                        } else {
                            return false;
                        }
                    }
                }
            }

            i += 1;
        }

        false
    };

    //let cores = opt.cores;
    //let broker_port = opt.broker_port;
    //let remote_broker_addr = opt.remote_broker_addr;
    let input_dirs = opt.input;
    let output_dir = opt.output;
    let token_files = opt.tokens;
    let timeout_ms = opt.timeout;

    println!("Workdir: {:?}", workdir.to_string_lossy().to_string());

    unsafe { START_TIME = current_time() };

    let shmem_provider = StdShMemProvider::new().expect("Failed to init shared memory");

    let monitor = MultiMonitor::new(|s| {
        println!("{}, diffing: {:x?}", s, opcodes.borrow())
    });
    let mut mgr = SimpleEventManager::new(monitor);

    //let mut run_client = |state: Option<StdState<_, _, _, _, _, _>>, mut mgr, _core_id| {
    let stdout1 = StdOutObserver::new("StdOutObserver1".into());
    let stdout2 = StdOutObserver::new("StdOutObserver2".into());

    let mut diff_feedback = DiffFeedback::new("differ", &stdout1, &stdout2, |o1, o2| {
        unsafe { EXECS += 1 };
        if observers_cmp(o1.stdout.as_ref().unwrap(), o2.stdout.as_ref().unwrap()) {
            //eprintln!("DIFFFFFF");
            //eprintln!(">>> {} {}", o1.name(), &o1.stdout.as_ref().unwrap());
            //eprintln!(">>> {} {}", o2.name(), &o2.stdout.as_ref().unwrap());
            DiffResult::Diff
        } else {
            DiffResult::Equal
        }
    })
    .unwrap();

    //let mut obj_dedup = EVMTypeHashFeedback::new("dedup", false, &stdout1);
    //let mut objective = feedback_and_fast!(diff_feedback, obj_dedup);
    let mut objective = diff_feedback;

    let mut th_feedback = EVMTypeHashFeedback::new("typehash", true, &stdout1);

    // If not restarting, create a State from scratch
    let mut state = //state.unwrap_or_else(|| {
            StdState::new(
                // RNG
                StdRand::with_seed(current_nanos()),
                // Corpus that will be evolved, we keep it in memory for performance
                InMemoryCorpus::new(),
                // Corpus in which we store solutions (crashes in this example),
                // on disk so the user can get them after stopping the fuzzer
                OnDiskCorpus::new(output_dir.clone()).unwrap(),
                // States of the feedbacks.
                // They are the data related to the feedbacks that you want to persist in the State.
                &mut th_feedback,
                &mut objective,
            )
            .unwrap()
        //});
        ;

    // Create a dictionary if not existing
    if state.metadata().get::<Tokens>().is_none() {
        for tokens_file in &token_files {
            state.add_metadata(Tokens::from_file(tokens_file).unwrap());
        }
    }

    // A minimization+queue policy to get testcasess from the corpus
    let scheduler = QueueScheduler::new();

    // A fuzzer with feedbacks and a corpus scheduler
    let mut fuzzer = StdFuzzer::new(scheduler, th_feedback, objective);

    // ./go-ethereum/build/bin/evm --json --sender 0x00 --receiver 0x00 --gas 0x1337 --code 7f30c0b3e1400000004e4e4e4e4e4e2dcecececeffffff7fcece1d3b3b3b3b3b3b3b3b run
    let ce1 = CommandExecutor::builder()
        .program("./go-ethereum/build/bin/evm")
        .args(&[
            "--json",
            "--sender",
            "0x00",
            "--receiver",
            "0x00",
            "--gas",
            "0x1337",
            "--code",
        ])
        .arg_input_arg()
        .arg("run")
        .stdout_observer("StdOutObserver1".into())
        .build(tuple_list!(stdout1))
        .unwrap();

    // ./openethereum/target/release/openethereum-evm --chain ./openethereum/crates/ethcore/res/chainspec/test/istanbul_test.json --gas 1337 --json --code 7f30c0b3e1400000004e4e4e4e4e4e2dcecececeffffff7fcece1d3b3b3b3b3b3b3b3b
    let ce2 = CommandExecutor::builder()
        .program("./openethereum/target/release/openethereum-evm")
        .args(&[
            "--chain",
            "./openethereum/crates/ethcore/res/chainspec/test/istanbul_test.json",
            "--gas",
            "1337",
            "--json",
            "--code",
        ])
        .arg_input_arg()
        .stdout_observer("StdOutObserver2".into())
        .build(tuple_list!(stdout2))
        .unwrap();

    let mut diff_executor = DiffExecutor::new(ce1, ce2);

    // Setup a basic mutator
    let mutator = StdScheduledMutator::new(havoc_mutations().merge(tokens_mutations()));
    let mutational = StdMutationalStage::new(mutator);

    // The order of the stages matter!
    let mut stages = tuple_list!(mutational);

    // In case the corpus is empty (on first run), reset
    if state.corpus().count() < 1 {
        if input_dirs.is_empty() {
            // Generator of printable bytearrays of max size 32
            let mut generator = RandHexGenerator::new(32);

            // Generate 8 initial inputs
            state
                .generate_initial_inputs(
                    &mut fuzzer,
                    &mut diff_executor,
                    &mut generator,
                    &mut mgr,
                    8,
                )
                .expect("Failed to generate the initial corpus");
            println!(
                "We imported {} inputs from the generator.",
                state.corpus().count()
            );
        } else {
            println!("Loading from {:?}", &input_dirs);
            // Load from disk
            state
                .load_initial_inputs(&mut fuzzer, &mut diff_executor, &mut mgr, &input_dirs)
                .unwrap_or_else(|_| panic!("Failed to load initial corpus at {:?}", &input_dirs));
            println!("We imported {} inputs from disk.", state.corpus().count());
        }
    }

    if opt.nofuzz {
        return;
    }

    fuzzer
        .fuzz_loop(&mut stages, &mut diff_executor, &mut state, &mut mgr)
        .unwrap();
    //Ok(())
    //};

    /*match Launcher::builder()
        .shmem_provider(shmem_provider)
        .configuration(EventConfig::from_name("default"))
        .monitor(monitor)
        .run_client(&mut run_client)
        .cores(&cores)
        .broker_port(broker_port)
        .remote_broker_addr(remote_broker_addr)
        //.stdout_file(Some("/dev/null"))
        .build()
        .launch()
    {
        Ok(_) | Err(Error::ShuttingDown) => (),
        Err(e) => panic!("{:?}", e),
    };*/
}
