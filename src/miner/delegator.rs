// Copyright 2017 The Mugle Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Internal module responsible for job delegation, creating hashes
//! and sending them to the plugin's internal queues. Used internally
//!
//!

use std::sync::{Arc, RwLock};
use std::{thread, time};
use std::mem::transmute;

use rand::{self, Rng};
use byteorder::{ByteOrder, BigEndian};
use blake2::blake2b::Blake2b;
use env_logger;

use cuckoo_sys::manager::PluginLibrary;
use error::error::CuckooMinerError;
use CuckooMinerJobHandle;
use CuckooMinerSolution;

/// From mugle
/// The target is the 8-bytes hash block hashes must be lower than.
const MAX_TARGET: [u8; 8] = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];

type JobSharedDataType = Arc<RwLock<JobSharedData>>;
type JobControlDataType = Arc<RwLock<JobControlData>>;
type PluginLibrariesDataType = Arc<RwLock<Vec<PluginLibrary>>>;

/// Data intended to be shared across threads
pub struct JobSharedData {
	/// ID of the current running job (not currently used)
	pub job_id: u32,

	/// The part of the header before the nonce, which this
	/// module will mutate in search of a solution
	pub pre_nonce: String,

	/// The part of the header after the nonce
	pub post_nonce: String,

	/// The target difficulty. Only solutions >= this
	/// target will be put into the output queue
	pub difficulty: u64,

	/// Output solutions
	pub solutions: Vec<CuckooMinerSolution>,
}

impl Default for JobSharedData {
	fn default() -> JobSharedData {
		JobSharedData {
			job_id: 0,
			pre_nonce: String::from(""),
			post_nonce: String::from(""),
			difficulty: 0,
			solutions: Vec::new(),
		}
	}
}

impl JobSharedData {
	pub fn new(job_id: u32, pre_nonce: &str, post_nonce: &str, difficulty: u64) -> JobSharedData {
		JobSharedData {
			job_id: job_id,
			pre_nonce: String::from(pre_nonce),
			post_nonce: String::from(post_nonce),
			difficulty: difficulty,
			solutions: Vec::new(),
		}
	}
}

/// an internal structure to flag job control

pub struct JobControlData {
	/// Whether the mining job is running
	pub stop_flag: bool,

	/// Whether all plugins have stopped
	pub has_stopped: bool,
}

impl Default for JobControlData {
	fn default() -> JobControlData {
		JobControlData {
			stop_flag: false,
			has_stopped: false,
		}
	}
}

/// Internal structure which controls and runs processing jobs.
///
///

pub struct Delegator {
	/// Data which is shared across all threads
	shared_data: JobSharedDataType,

	/// Job control flags which are shared across threads
	control_data: JobControlDataType,

	/// Loaded Plugin Library
	libraries: PluginLibrariesDataType,
}

impl Delegator {
	/// Create a new job delegator

	pub fn new(job_id: u32, pre_nonce: &str, post_nonce: &str, difficulty: u64, libraries:Vec<PluginLibrary>) -> Delegator {
		Delegator {
			shared_data: Arc::new(RwLock::new(JobSharedData::new(
				job_id,
				pre_nonce,
				post_nonce,
				difficulty,
			))),
			control_data: Arc::new(RwLock::new(JobControlData::default())),
			libraries: Arc::new(RwLock::new(libraries)),
		}
	}

	/// Starts the job loop, and initialises the internal plugin

	pub fn start_job_loop(self, hash_header: bool) -> Result<CuckooMinerJobHandle, CuckooMinerError> {
		let _=env_logger::init();
		// this will block, waiting until previous job is cleared
		// call_cuckoo_stop_processing();

		let shared_data = self.shared_data.clone();
		let control_data = self.control_data.clone();
		let jh_library = self.libraries.clone();

		thread::spawn(move || {
			let result = self.job_loop(hash_header);
			if let Err(e) = result {
				error!("Error in job loop: {:?}", e);
			}
		});
		Ok(CuckooMinerJobHandle {
			shared_data: shared_data,
			control_data: control_data,
			library: jh_library,
		})
	}

	/// Helper to convert a hex string

	fn from_hex_string(&self, in_str: &str) -> Vec<u8> {
		let mut bytes = Vec::new();
		for i in 0..(in_str.len() / 2) {
			let res = u8::from_str_radix(&in_str[2 * i..2 * i + 2], 16);
			match res {
				Ok(v) => bytes.push(v),
				Err(e) => println!("Problem with hex: {}", e),
			}
		}
		bytes
	}

	/// As above, except doesn't hash the result
	fn header_data(&self, pre_nonce: &str, post_nonce: &str, nonce: u64) -> Vec<u8> {
		// Turn input strings into vectors
		let mut pre_vec = self.from_hex_string(pre_nonce);
		let mut post_vec = self.from_hex_string(post_nonce);

		let mut nonce_bytes = [0; 8];
		BigEndian::write_u64(&mut nonce_bytes, nonce);
		let mut nonce_vec = nonce_bytes.to_vec();

		// Generate new header
		pre_vec.append(&mut nonce_vec);
		pre_vec.append(&mut post_vec);

		pre_vec
	}
	/// helper that generates a nonce and returns a header

	fn get_next_header_data_hashed(&self, pre_nonce: &str, post_nonce: &str) -> (u64, Vec<u8>) {
		// Generate new nonce
		let nonce: u64 = rand::OsRng::new().unwrap().gen();
		let mut blake2b = Blake2b::new(32);
		blake2b.update(&self.header_data(pre_nonce, post_nonce, nonce));

		let mut ret = [0; 32];
		ret.copy_from_slice(blake2b.finalize().as_bytes());
		(nonce, ret.to_vec())
	}

	/// as above, except doesn't hash the result
	fn get_next_header_data(&self, pre_nonce: &str, post_nonce: &str) -> (u64, Vec<u8>) {
		let nonce: u64 = rand:: OsRng::new().unwrap().gen();
		(nonce, self.header_data(pre_nonce, post_nonce, nonce))
	}

	/// Helper to determing whether a solution meets a target difficulty
	/// based on same algorithm from mugle

	fn meets_difficulty(&self, in_difficulty: u64, sol: CuckooMinerSolution) -> bool {
		let max_target = BigEndian::read_u64(&MAX_TARGET);
		let num = BigEndian::read_u64(&sol.hash()[0..8]);
		max_target / num >= in_difficulty
	}

	/// The main job loop. Pushes hashes to the plugin and reads solutions
	/// from the queue, putting them into the job's output queue. Continues
	/// until another thread sets the is_running flag to false

	fn job_loop(self, hash_header: bool) -> Result<(), CuckooMinerError> {
		// keep some unchanging data here, can move this out of shared
		// object later if it's not needed anywhere else
		let pre_nonce: String;
		let post_nonce: String;
		// generate an identifier to ensure we're only reading our
		// jobs from the queue
		let queue_id: u32 = rand::OsRng::new().unwrap().gen();
		let difficulty;
		{
			let s = self.shared_data.read().unwrap();
			pre_nonce = s.pre_nonce.clone();
			post_nonce = s.post_nonce.clone();
			difficulty = s.difficulty;
		}
		debug!(
			"Cuckoo-miner: Searching for solution >= difficulty {}",
			difficulty
		);
	
		for l in self.libraries.read().unwrap().iter() {
			l.call_cuckoo_start_processing();
		}

		debug!("Cuckoo Miner Job loop processing");
		let mut solution = CuckooMinerSolution::new();

		loop {
			// Check if it's time to stop
			{
				let s = self.control_data.read().unwrap();
				if s.stop_flag {
					break;
				}
			}
			for l in self.libraries.read().unwrap().iter() {
				while l.call_cuckoo_is_queue_under_limit() == 1 {
					let (nonce, data) = match hash_header {
						true => self.get_next_header_data_hashed(&pre_nonce, &post_nonce),
						false => self.get_next_header_data(&pre_nonce, &post_nonce),
					};
					// TODO: make this a serialise operation instead
					let nonce_bytes: [u8; 8] = unsafe { transmute(nonce.to_be()) };
					l.call_cuckoo_push_to_input_queue(queue_id, &data, &nonce_bytes);
				}
			}

			let mut plugin_index=0;
			for l in self.libraries.read().unwrap().iter() {
				let mut qid:u32 = 0;
				while l.call_cuckoo_read_from_output_queue(
					&mut qid,
					&mut solution.solution_nonces,
					&mut solution.cuckoo_size,
					&mut solution.nonce,
				) != 0
				{
					// TODO: make this a serialise operation instead
					let nonce = unsafe { transmute::<[u8; 8], u64>(solution.nonce) }.to_be();

					if self.meets_difficulty(difficulty, solution) && qid == queue_id {
						debug!(
							"Cuckoo-miner plugin[{}]: Solution Found for Nonce:({}), {:?}",
							plugin_index,
							nonce,
							solution
						);
						let mut s = self.shared_data.write().unwrap();
						s.solutions.push(solution.clone());
						plugin_index+=1;
					}

				}
			}
			//avoid busy wait 
			let sleep_dur = time::Duration::from_millis(100);
			thread::sleep(sleep_dur);
		}

		// Do any cleanup
		for l in self.libraries.read().unwrap().iter() {
			l.call_cuckoo_stop_processing();
		}
		for l in self.libraries.read().unwrap().iter() {
			//wait for internal processing to finish
			while l.call_cuckoo_has_processing_stopped()==0{
				thread::sleep(time::Duration::from_millis(1));
			};
			l.call_cuckoo_reset_processing();
		}
		let mut s = self.control_data.write().unwrap();
		s.has_stopped=true;
		Ok(())
	}
}
