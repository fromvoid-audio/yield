use super::{Job, PayloadStageIdentity, PayloadStagePhase, SelectionError};
use crate::payload_selection::{
    SelectedPayload, catalog,
    protocol::{self, staging},
};

pub(super) fn attempt(job: &Job) -> Result<super::super::VerifiedPayload, SelectionError> {
    job.traffic.check()?;
    let mut working = super::super::Ledger(staging::OWNER_WORKING_CREDIT);
    let raw = staging::metadata(&job.transport, protocol::CATALOG, job.traffic.clone())?;
    working.charge(raw.capacity())?;
    job.traffic.check()?;
    let release = catalog::select(&raw, &job.installed, job.channel)?;
    drop(raw);
    let gate_url = protocol::gate_url(&release.build_id)?;
    let gate = staging::metadata(&job.transport, &gate_url, job.traffic.clone())?;
    working.charge(gate.capacity())?;
    job.traffic.check()?;
    let requests = protocol::gate_requests(&gate, &release, job.channel, job.installed.platform)?;
    drop(gate);
    let selection = SelectedPayload {
        release,
        installed: job.installed.clone(),
        channel: job.channel,
        lease: job.lease.clone(),
        requests,
    };
    {
        let identity = selection.identity()?;
        let mut state = job.state.lock().map_err(|_| SelectionError::Unavailable)?;
        state.identity = Some(PayloadStageIdentity {
            version: identity.version.into(),
            build_id: identity.build_id.into(),
            build_number: identity.build_number,
            commit: identity.commit,
            channel: identity.channel,
            platform: identity.platform,
        });
        state.phase = PayloadStagePhase::Downloading;
    }
    let fetch = |index: usize| {
        job.traffic.check()?;
        let request = selection
            .requests
            .get(index)
            .ok_or(SelectionError::Invalid)?;
        let file = selection
            .release
            .files
            .iter()
            .find(|f| f.name == request.name())
            .ok_or(SelectionError::Invalid)?;
        staging::asset(&job.transport, request, file, job.traffic.clone())
    };
    let checksum = fetch(1)?;
    working.charge(checksum.capacity())?;
    let signature = fetch(2)?;
    working.charge(signature.capacity())?;
    let bytes = fetch(0)?;
    job.phase(PayloadStagePhase::Verifying)?;
    let payload = super::super::verify_inner(
        selection,
        bytes,
        checksum,
        signature,
        job.key.clone(),
        job.deadline,
    )?;
    job.traffic.check()?;
    #[cfg(test)]
    if let Some(witness) = &job.witness {
        assert_eq!(payload.bytes, witness.expected);
        witness
            .retained
            .store(true, std::sync::atomic::Ordering::Release);
    }
    {
        let mut state = job.state.lock().map_err(|_| SelectionError::Unavailable)?;
        job.traffic.check()?;
        state.archive_len = Some(payload.archive_len()?);
        state.archive_sha256 = Some(payload.archive_sha256()?);
        state.build_date = Some(payload.build_date()?.into());
        state.phase = PayloadStagePhase::Staged;
    }
    Ok(payload)
}
