use std::marker::PhantomData;
use std::collections::{BTreeSet, BTreeMap};
use key_server_cluster::{Error, NodeId, SessionId, SessionMeta};

/// Job executor.
pub trait JobExecutor {
	type PartialJobRequest;
	type PartialJobResponse;
	type JobResponse;

	/// Prepare job request for given node.
	fn prepare_partial_request(&self) -> Result<Self::PartialJobRequest, Error>;
	/// Process partial request.
	fn process_partial_request(&self, partial_request: Self::PartialJobRequest) -> Result<Self::PartialJobResponse, Error>;
	/// Check partial response of given node.
	fn check_partial_response(&self, partial_response: &Self::PartialJobResponse) -> Result<bool, Error>;
	/// Compute final job response.
	fn compute_response(&self, partial_responses: &BTreeMap<NodeId, Self::PartialJobResponse>) -> Result<Self::JobResponse, Error>;
}

/// Jobs transport.
pub trait JobTransport {
	type PartialJobRequest;
	type PartialJobResponse;

	/// Send partial request to given node.
	fn send_partial_request(&self, node: &NodeId, request: Self::PartialJobRequest) -> Result<(), Error>;
	/// Send partial request to given node.
	fn send_partial_response(&self, node: &NodeId, response: Self::PartialJobResponse) -> Result<(), Error>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// Current state of job session.
pub enum JobSessionState {
	/// Session is inactive.
	Inactive,
	/// Session is active.
	Active,
	/// Session is finished.
	Finished,
	/// Session has failed.
	Failed,
}

/// Basic request-response session on a set of nodes.
pub struct JobSession<'a, Executor: JobExecutor, Transport> where Transport: JobTransport<PartialJobRequest = Executor::PartialJobRequest, PartialJobResponse = Executor::PartialJobResponse> {
	/// Session meta.
	meta: &'a SessionMeta,
	/// Job executor.
	executor: Executor,
	/// Jobs transport.
	transport: Transport,
	/// Session data.
	data: JobSessionData<Executor::PartialJobResponse>,
	//// PartialJobRequest dummy.
	// dummy: PhantomData<PartialJobRequest>,
}

/// Data of job session.
struct JobSessionData<PartialJobResponse> {
	/// Session state.
	state: JobSessionState,
	/// Mutable session data.
	active_data: Option<ActiveJobSessionData<PartialJobResponse>>,
}

/// Active job session data.
struct ActiveJobSessionData<PartialJobResponse> {
	/// Active partial requests.
	requests: BTreeSet<NodeId>,
	/// Rejects to partial requests.
	rejects: BTreeSet<NodeId>,
	/// Received partial responses.
	responses: BTreeMap<NodeId, PartialJobResponse>,
}

impl<'a, Executor, Transport> JobSession<'a, Executor, Transport> where Executor: JobExecutor, Transport: JobTransport<PartialJobRequest = Executor::PartialJobRequest, PartialJobResponse = Executor::PartialJobResponse> {
	/// Create new session.
	pub fn new(meta: &'a SessionMeta, executor: Executor, transport: Transport) -> Self {
		JobSession {
			meta: meta,
			executor: executor,
			transport: transport,
			data: JobSessionData {
				state: JobSessionState::Inactive,
				active_data: None,
			},
		}
	}

	#[cfg(test)]
	/// Get transport reference.
	pub fn transport(&self) -> &Transport {
		&self.transport
	}

	/// Get job state.
	pub fn state(&self) -> JobSessionState {
		self.data.state
	}

	/// Get active requests.
	pub fn requests(&self) -> &BTreeSet<NodeId> {
		debug_assert!(self.meta.self_node_id == self.meta.master_node_id);

		&self.data.active_data.as_ref()
			.expect("requests is only called on master nodes after initialization; on master nodes active_data is filled during initialization; qed")
			.requests
	}

	/// Get job result.
	pub fn result(&self) -> Result<Executor::JobResponse, Error> {
		debug_assert!(self.meta.self_node_id == self.meta.master_node_id);

		if self.data.state != JobSessionState::Finished {
			return Err(Error::InvalidStateForRequest);
		}

		self.executor.compute_response(&self.data.active_data.as_ref()
			.expect("requests is only called on master nodes; on master nodes active_data is filled during initialization; qed")
			.responses)
	}

	/// Initialize.
	pub fn initialize(&mut self, mut nodes: BTreeSet<NodeId>) -> Result<(), Error> {		
		debug_assert!(self.meta.self_node_id == self.meta.master_node_id);
		debug_assert!(nodes.len() >= self.meta.threshold + 1);

		if self.data.state != JobSessionState::Inactive {
			return Err(Error::InvalidStateForRequest);
		}

		// send requests to slave nodes
		let mut waits_for_self = false;
		let active_data = ActiveJobSessionData {
			requests: nodes,
			rejects: BTreeSet::new(),
			responses: BTreeMap::new(),
		};
		for node in &active_data.requests {
			if node != &self.meta.self_node_id {
				self.transport.send_partial_request(&node, self.executor.prepare_partial_request()?)?;
			} else {
				waits_for_self = true;
			}
		}

		// update state
		self.data.active_data = Some(active_data);
		self.data.state = JobSessionState::Active;

		// if we are waiting for response from self => do it
		if waits_for_self {
			let partial_response = self.executor.process_partial_request(self.executor.prepare_partial_request()?)?;
			self.on_partial_response(&self.meta.self_node_id, partial_response)?;
		}

		Ok(())
	}

	/// When partial request is received by slave node.
	pub fn on_partial_request(&mut self, node: &NodeId, request: Executor::PartialJobRequest) -> Result<(), Error> {
		if node != &self.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}
		if self.meta.self_node_id == self.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}
		if self.data.state != JobSessionState::Inactive && self.data.state != JobSessionState::Finished {
			return Err(Error::InvalidStateForRequest);
		}

		self.data.state = JobSessionState::Finished;
		self.transport.send_partial_response(node, self.executor.process_partial_request(request)?)
	}

	/// When partial request is received by master node.
	pub fn on_partial_response(&mut self, node: &NodeId, response: Executor::PartialJobResponse) -> Result<(), Error> {
		if self.meta.self_node_id != self.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}
		if self.data.state != JobSessionState::Active && self.data.state != JobSessionState::Finished {
			return Err(Error::InvalidStateForRequest);
		}

		let active_data = self.data.active_data.as_mut()
			.expect("on_partial_response is only called on master nodes; on master nodes active_data is filled during initialization; qed");
		if !active_data.requests.remove(node) {
			return Err(Error::InvalidNodeForRequest);
		}
		
		if !self.executor.check_partial_response(&response).unwrap_or(false) {
			active_data.rejects.insert(node.clone());
			if active_data.requests.len() + active_data.responses.len() >= self.meta.threshold + 1 {
				return Ok(());
			}

			self.data.state = JobSessionState::Failed;
			Err(Error::ConsensusUnreachable)
		} else {
			active_data.responses.insert(node.clone(), response);

			if active_data.responses.len() < self.meta.threshold + 1 {
				return Ok(());
			}

			self.data.state = JobSessionState::Finished;
			Ok(())
		}
	}

	/// When node is timeouted.
	pub fn on_node_timeout(&mut self, node: &NodeId) -> Result<(), Error> {
		if self.meta.self_node_id != self.meta.master_node_id {
			if node != &self.meta.master_node_id {
				return Ok(());
			}

			self.data.state = JobSessionState::Failed;
			return Err(Error::NodeDisconnected);
		}

		let active_data = self.data.active_data.as_mut()
			.expect("we have checked that we are on master node; on master nodes active_data is filled during initialization; qed");
		if active_data.rejects.contains(node) {
			return Ok(());
		}
		if active_data.requests.remove(node) || active_data.responses.remove(node).is_some() {
			active_data.rejects.insert(node.clone());
			if active_data.requests.len() + active_data.responses.len() >= self.meta.threshold + 1 {
				return Ok(());
			}

			self.data.state = JobSessionState::Failed;
			return Err(Error::NodeDisconnected);
		}

		Ok(())
	}

	/// When session timeouted.
	pub fn on_session_timeout(&mut self) {
		self.data.state = JobSessionState::Failed;
	}
}


#[cfg(test)]
mod tests {
	use std::collections::{VecDeque, BTreeMap};
	use parking_lot::Mutex;
	use ethkey::Public;
	use key_server_cluster::{Error, NodeId, SessionId, SessionMeta, DocumentKeyShare};
	use super::{JobExecutor, JobTransport, JobSession, JobSessionState};

	struct SquaredSumJobExecutor;

	impl JobExecutor for SquaredSumJobExecutor {
		type PartialJobRequest = u32;
		type PartialJobResponse = u32;
		type JobResponse = u32;

		fn prepare_partial_request(&self) -> Result<u32, Error> { Ok(2) }
		fn process_partial_request(&self, r: u32) -> Result<u32, Error> { Ok(r * r) }
		fn check_partial_response(&self, r: &u32) -> Result<bool, Error> { Ok(r % 2 == 0) }
		fn compute_response(&self, r: &BTreeMap<NodeId, u32>) -> Result<u32, Error> { Ok(r.values().fold(0, |v1, v2| v1 + v2)) }
	}

	#[derive(Default)]
	struct DummyJobTransport {
		pub requests: Mutex<VecDeque<(NodeId, u32)>>,
		pub responses: Mutex<VecDeque<(NodeId, u32)>>,
	}

	impl DummyJobTransport {
		pub fn response(&self) -> (NodeId, u32) {
			self.responses.lock().pop_front().unwrap()
		}
	}

	impl JobTransport for DummyJobTransport {
		type PartialJobRequest = u32;
		type PartialJobResponse = u32;

		fn send_partial_request(&self, node: &NodeId, request: u32) -> Result<(), Error> { self.requests.lock().push_back((node.clone(), request)); Ok(()) }
		fn send_partial_response(&self, node: &NodeId, response: u32) -> Result<(), Error> { self.responses.lock().push_back((node.clone(), response)); Ok(()) }
	}

	fn make_master_session_meta(threshold: usize) -> SessionMeta {
		SessionMeta { id: SessionId::default(), master_node_id: NodeId::from(1), self_node_id: NodeId::from(1), threshold: threshold }
	}

	fn make_slave_session_meta(threshold: usize) -> SessionMeta {
		SessionMeta { id: SessionId::default(), master_node_id: NodeId::from(1), self_node_id: NodeId::from(2), threshold: threshold }
	}

	#[test]
	fn job_initialize_fails_if_not_inactive() {
		let meta = make_master_session_meta(0);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1)].into_iter().collect()).unwrap();
		assert_eq!(job.initialize(vec![Public::from(1)].into_iter().collect()).unwrap_err(), Error::InvalidStateForRequest);
	}

	#[test]
	fn job_initialization_leads_to_finish_if_single_node_is_required() {
		let meta = make_master_session_meta(0);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Finished);
		assert_eq!(job.result(), Ok(4));
	}

	#[test]
	fn job_initialization_does_not_leads_to_finish_if_single_other_node_is_required() {
		let meta = make_master_session_meta(0);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
	}

	#[test]
	fn job_request_fails_if_comes_from_non_master_node() {
		let meta = make_slave_session_meta(0);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		assert_eq!(job.on_partial_request(&NodeId::from(3), 2).unwrap_err(), Error::InvalidMessage);
	}

	#[test]
	fn job_request_fails_if_comes_to_master_node() {
		let meta = make_master_session_meta(0);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		assert_eq!(job.on_partial_request(&NodeId::from(1), 2).unwrap_err(), Error::InvalidMessage);
	}

	#[test]
	fn job_request_fails_if_comes_to_failed_state() {
		let meta = make_slave_session_meta(0);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.on_session_timeout();
		assert_eq!(job.on_partial_request(&NodeId::from(1), 2).unwrap_err(), Error::InvalidStateForRequest);
	}

	#[test]
	fn job_request_succeeds_if_comes_to_finished_state() {
		let meta = make_slave_session_meta(0);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.on_partial_request(&NodeId::from(1), 2).unwrap();
		assert_eq!(job.transport().response(), (NodeId::from(1), 4));
		assert_eq!(job.state(), JobSessionState::Finished);
		job.on_partial_request(&NodeId::from(1), 3).unwrap();
		assert_eq!(job.transport().response(), (NodeId::from(1), 9));
		assert_eq!(job.state(), JobSessionState::Finished);
	}

	#[test]
	fn job_response_fails_if_comes_to_slave_node() {
		let meta = make_slave_session_meta(0);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		assert_eq!(job.on_partial_response(&NodeId::from(1), 2).unwrap_err(), Error::InvalidMessage);
	}

	#[test]
	fn job_response_fails_if_comes_to_failed_state() {
		let meta = make_master_session_meta(0);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(2)].into_iter().collect()).unwrap();
		job.on_session_timeout();
		assert_eq!(job.on_partial_response(&NodeId::from(2), 2).unwrap_err(), Error::InvalidStateForRequest);
	}

	#[test]
	fn job_response_fails_if_comes_from_unknown_node() {
		let meta = make_master_session_meta(0);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.on_partial_response(&NodeId::from(3), 2).unwrap_err(), Error::InvalidNodeForRequest);
	}

	#[test]
	fn job_response_leads_to_failure_if_too_few_nodes_left() {
		let meta = make_master_session_meta(1);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		assert_eq!(job.on_partial_response(&NodeId::from(2), 3).unwrap_err(), Error::ConsensusUnreachable);
		assert_eq!(job.state(), JobSessionState::Failed);
	}

	#[test]
	fn job_response_succeeds() {
		let meta = make_master_session_meta(2);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2), Public::from(3)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		job.on_partial_response(&NodeId::from(2), 2).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
	}

	#[test]
	fn job_response_leads_to_finish() {
		let meta = make_master_session_meta(1);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		job.on_partial_response(&NodeId::from(2), 2).unwrap();
		assert_eq!(job.state(), JobSessionState::Finished);
	}

	#[test]
	fn job_node_timeout_ignored_when_slave_disconnects_from_slave() {
		let meta = make_slave_session_meta(1);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		assert_eq!(job.state(), JobSessionState::Inactive);
		job.on_node_timeout(&NodeId::from(3)).unwrap();
		assert_eq!(job.state(), JobSessionState::Inactive);
	}

	#[test]
	fn job_node_timeout_leads_to_fail_when_slave_disconnects_from_master() {
		let meta = make_slave_session_meta(1);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		assert_eq!(job.state(), JobSessionState::Inactive);
		assert_eq!(job.on_node_timeout(&NodeId::from(1)).unwrap_err(), Error::NodeDisconnected);
		assert_eq!(job.state(), JobSessionState::Failed);
	}

	#[test]
	fn job_node_timeout_ignored_when_disconnects_from_rejected() {
		let meta = make_master_session_meta(1);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2), Public::from(3)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		job.on_partial_response(&NodeId::from(2), 3).unwrap();
		job.on_node_timeout(&NodeId::from(2)).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
	}

	#[test]
	fn job_node_timeout_ignored_when_disconnects_from_unknown() {
		let meta = make_master_session_meta(1);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		job.on_node_timeout(&NodeId::from(3)).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
	}

	#[test]
	fn job_node_timeout_ignored_when_disconnects_from_requested_and_enough_nodes_left() {
		let meta = make_master_session_meta(1);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2), Public::from(3)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		job.on_node_timeout(&NodeId::from(3)).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
	}

	#[test]
	fn job_node_timeout_leads_to_fail_when_disconnects_from_requested_and_not_enough_nodes_left() {
		let meta = make_master_session_meta(1);
		let mut job = JobSession::new(&meta, SquaredSumJobExecutor, DummyJobTransport::default());
		job.initialize(vec![Public::from(1), Public::from(2)].into_iter().collect()).unwrap();
		assert_eq!(job.state(), JobSessionState::Active);
		assert_eq!(job.on_node_timeout(&NodeId::from(2)).unwrap_err(), Error::NodeDisconnected);
		assert_eq!(job.state(), JobSessionState::Failed);
	}
}
