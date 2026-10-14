//! Per-task verification state and the rules that settle it.
//!
//! A result produced by the coordinator itself is authoritative. Otherwise the job's
//! verification mode decides how many agreeing executions a task needs: one for `None`,
//! one plus an optional recompute for `Sample`, two from distinct executors for `Full`. When
//! the coordinator can run the task, a sampled share of `Full` tasks also gets its own
//! recompute: two agreeing peers may be two identities of one attacker.

use std::sync::Arc;

use nuee_proto::{EndpointId, Hash, VerifyMode};

use crate::Compare;

/// How outputs of one job are compared.
#[derive(Clone)]
pub(crate) enum Cmp {
    /// Hash equality.
    Exact,
    /// Every output is acceptable.
    Any,
    /// Injected tolerance comparator.
    Custom(Arc<dyn Compare>),
}

impl Cmp {
    pub(crate) fn same(&self, a: &Exec, b: &Exec) -> bool {
        if a.hash == b.hash {
            return true;
        }
        match self {
            Cmp::Exact => false,
            Cmp::Any => true,
            Cmp::Custom(c) => match (&a.output, &b.output) {
                (Some(x), Some(y)) => c.same(x, y),
                _ => false,
            },
        }
    }
}

/// One execution of a task.
#[derive(Clone)]
pub(crate) struct Exec {
    pub by: EndpointId,
    pub hash: Hash,
    /// Output bytes; absent for tasks restored with `mark_done`.
    pub output: Option<Arc<[u8]>>,
    /// Milliseconds from the lease to the result, as the coordinator saw them.
    pub ms: Option<u64>,
}

/// Job-level settings needed to settle a task.
pub(crate) struct Ctx<'a> {
    pub mode: VerifyMode,
    pub cmp: &'a Cmp,
    /// The coordinator's own identity.
    pub local: EndpointId,
    /// The coordinator can run tasks itself.
    pub can_local: bool,
}

/// Consequences of settling a task, applied by the coordinator.
#[derive(Default)]
pub(crate) struct Resolution {
    pub ban: Vec<EndpointId>,
    pub credit: Vec<EndpointId>,
    pub mismatch: Option<EndpointId>,
    pub need_local: bool,
    /// Peers in a disagreement nobody could arbitrate: no ban, but their other unconfirmed
    /// work is re-executed and what they produce from now on needs two agreeing executions.
    pub suspects: Vec<EndpointId>,
}

/// What callers compare before and after a change.
pub(crate) type Snapshot = (Option<(Hash, EndpointId)>, bool);

#[derive(Default)]
pub(crate) struct TaskState {
    /// Output currently accepted.
    pub accepted: Option<Exec>,
    /// The accepted output was confirmed independently.
    pub verified: bool,
    /// Results not yet settled.
    pub results: Vec<Exec>,
    /// Peers that ran or are running this task.
    pub runners: Vec<EndpointId>,
    /// Peers that failed or timed out on this task.
    pub soft: Vec<EndpointId>,
    /// Entries of this task in the job queue.
    pub queued: u32,
    /// Peer executions in progress.
    pub in_flight: u32,
    /// Waiting for a coordinator recompute.
    pub local_pending: bool,
    /// The coordinator is running this task.
    pub local_running: bool,
    /// A sampled verification is outstanding.
    pub verifying: bool,
    /// Needs two agreeing executions whatever the mode (after an unarbitrated mismatch).
    pub require_two: bool,
    /// Distinct peers (or the coordinator) that failed this task.
    pub attempts: u8,
    /// Who failed this task (at most `MAX_SOFT`): one identity counts once in `attempts`.
    pub failers: Vec<EndpointId>,
    /// Every failure, repeated ones included.
    pub fails: u16,
    /// Peers gave up on it: the coordinator runs it once itself before it is failed.
    pub last_chance: bool,
    /// A lease of this task ran past its deadline: the task is longer than the peer's
    /// other tasks, so its next leases get the job's own deadline.
    pub overran: bool,
    /// Gave up after too many failures.
    pub failed: bool,
    /// Listed among the job's known answers.
    pub known: bool,
    /// The accepted output was computed or confirmed by the coordinator itself. Only such
    /// an output may judge (ban) a differing peer or serve as a known answer (canary).
    pub local_checked: bool,
}

const MAX_SOFT: usize = 16;

impl TaskState {
    pub(crate) fn snapshot(&self) -> Snapshot {
        (
            self.accepted.as_ref().map(|a| (a.hash, a.by)),
            self.verified,
        )
    }

    fn two(&self, mode: VerifyMode) -> bool {
        mode == VerifyMode::Full || self.require_two
    }

    /// Peer executions still to schedule.
    pub(crate) fn want(&self, mode: VerifyMode, can_local: bool) -> u32 {
        if self.failed || self.local_pending || self.local_running || self.verified {
            return 0;
        }
        let have = u32::try_from(self.results.len())
            .unwrap_or(u32::MAX)
            .saturating_add(self.in_flight);
        if self.two(mode) {
            return 2u32.saturating_sub(have);
        }
        if self.accepted.is_none() || (self.verifying && !can_local) {
            return 1u32.saturating_sub(have);
        }
        0
    }

    /// A verification is still outstanding.
    pub(crate) fn pending_verification(&self) -> bool {
        !self.failed && (self.local_pending || self.verifying)
    }

    /// Settled for the purpose of finishing a job.
    pub(crate) fn settled(&self) -> bool {
        self.failed || (self.accepted.is_some() && !self.pending_verification())
    }

    pub(crate) fn note_soft(&mut self, peer: EndpointId) {
        if !self.soft.contains(&peer) && self.soft.len() < MAX_SOFT {
            self.soft.push(peer);
        }
    }

    /// Records a failure by `by`; returns whether attempts are now exhausted, either by
    /// `max` distinct identities or by `max * 3` failures in all.
    pub(crate) fn note_fail(&mut self, by: EndpointId, max: u8) -> bool {
        self.fails = self.fails.saturating_add(1);
        if !self.failers.contains(&by) {
            self.attempts = self.attempts.saturating_add(1);
            if self.failers.len() < MAX_SOFT {
                self.failers.push(by);
            }
        }
        self.exhausted(max)
    }

    fn exhausted(&self, max: u8) -> bool {
        self.attempts >= max || self.fails >= u16::from(max).saturating_mul(3)
    }

    /// Once attempts are exhausted, a peer that already failed this task never gets it again.
    pub(crate) fn barred(&self, peer: &EndpointId, max: u8) -> bool {
        self.exhausted(max) && self.failers.contains(peer)
    }

    /// Settles the task after a new entry in `results`.
    pub(crate) fn resolve(&mut self, ctx: &Ctx<'_>, sample_hit: bool) -> Resolution {
        let mut r = Resolution::default();
        if self.failed {
            self.results.clear();
            return r;
        }
        if let Some(i) = self.results.iter().position(|e| e.by == ctx.local) {
            let local = self.results.remove(i);
            for e in std::mem::take(&mut self.results) {
                judge(ctx, &local, e, &mut r);
            }
            match self.accepted.take() {
                Some(acc) if acc.by != ctx.local && !ctx.cmp.same(&acc, &local) => {
                    r.ban.push(acc.by);
                    self.accepted = Some(local);
                }
                Some(acc) => {
                    if acc.by != ctx.local && !self.verified {
                        r.credit.push(acc.by);
                    }
                    self.accepted = Some(acc);
                }
                None => self.accepted = Some(local),
            }
            self.verified = true;
            self.local_checked = true;
            self.local_pending = false;
            self.verifying = false;
            self.require_two = false;
            return r;
        }
        if self.verified {
            let results = std::mem::take(&mut self.results);
            if let Some(acc) = self.accepted.clone() {
                if self.local_checked {
                    for e in results {
                        judge(ctx, &acc, e, &mut r);
                    }
                } else {
                    // Verified by peer agreement only: never grounds for a ban.
                    let mut differ = Vec::new();
                    for e in results {
                        judge_unbanned(ctx, &acc, e, &mut r, &mut differ);
                    }
                    if !differ.is_empty() {
                        r.mismatch = differ.last().map(|e| e.by);
                        if ctx.can_local {
                            self.results = differ;
                            self.local_pending = true;
                            r.need_local = true;
                        } else {
                            r.suspects.push(acc.by);
                            r.suspects.extend(differ.iter().map(|e| e.by));
                        }
                    }
                }
            }
            return r;
        }
        if self.two(ctx.mode) {
            if self.results.len() < 2 {
                return r;
            }
            let mut pair = None;
            for (i, a) in self.results.iter().enumerate() {
                if self
                    .results
                    .iter()
                    .skip(i + 1)
                    .any(|b| b.by != a.by && ctx.cmp.same(a, b))
                {
                    pair = Some(i);
                    break;
                }
            }
            match pair {
                Some(i) => {
                    let keep = self.results.swap_remove(i);
                    if ctx.can_local && sample_hit {
                        // Two agreeing identities may be one attacker: the coordinator's own
                        // recompute judges every execution kept here.
                        self.accepted = Some(keep);
                        self.verifying = true;
                        self.local_pending = true;
                        r.need_local = true;
                        return r;
                    }
                    r.credit.push(keep.by);
                    let mut differ = Vec::new();
                    for e in std::mem::take(&mut self.results) {
                        judge_unbanned(ctx, &keep, e, &mut r, &mut differ);
                    }
                    self.accepted = Some(keep);
                    self.verified = true;
                    self.verifying = false;
                    self.require_two = false;
                }
                None => self.mismatch(ctx, &mut r),
            }
            return r;
        }
        if self.accepted.is_none() {
            if self.results.is_empty() {
                return r;
            }
            let first = self.results.remove(0);
            self.accepted = Some(first);
            if ctx.mode == VerifyMode::Sample && sample_hit {
                self.verifying = true;
                if ctx.can_local {
                    self.local_pending = true;
                    r.need_local = true;
                }
            }
        }
        if ctx.mode == VerifyMode::None {
            self.results.clear();
            return r;
        }
        let Some(acc) = self.accepted.clone() else {
            return r;
        };
        let mut rest = std::mem::take(&mut self.results).into_iter();
        let Some(second) = rest.next() else {
            return r;
        };
        if ctx.cmp.same(&acc, &second) {
            r.credit.push(acc.by);
            r.credit.push(second.by);
            self.verified = true;
            self.verifying = false;
            let mut differ = Vec::new();
            for e in rest {
                judge_unbanned(ctx, &acc, e, &mut r, &mut differ);
            }
        } else {
            self.results.push(second);
            self.results.extend(rest);
            self.mismatch(ctx, &mut r);
        }
        r
    }

    fn mismatch(&mut self, ctx: &Ctx<'_>, r: &mut Resolution) {
        r.mismatch = self.results.last().map(|e| e.by);
        if ctx.can_local {
            self.local_pending = true;
            r.need_local = true;
        } else {
            let unconfirmed = self.accepted.as_ref().filter(|_| !self.verified);
            for by in self.results.iter().chain(unconfirmed).map(|e| e.by) {
                if by != ctx.local && !r.suspects.contains(&by) {
                    r.suspects.push(by);
                }
            }
            self.discard();
        }
    }

    /// Drops every unconfirmed output; two new agreeing executions are then needed.
    fn discard(&mut self) {
        if !self.verified {
            self.accepted = None;
        }
        self.results.clear();
        self.require_two = true;
        self.verifying = false;
    }

    /// The coordinator could not recompute the task.
    pub(crate) fn local_failed(&mut self) {
        self.local_pending = false;
        if self.results.is_empty() {
            self.verifying = false;
        } else {
            self.discard();
        }
    }

    /// Forgets everything `peer` produced. Returns whether the task changed.
    pub(crate) fn purge(&mut self, peer: &EndpointId) -> bool {
        let before = self.results.len();
        self.results.retain(|e| e.by != *peer);
        let mut changed = before != self.results.len();
        if self.accepted.as_ref().is_some_and(|a| a.by == *peer) {
            self.accepted = None;
            self.verified = false;
            self.local_checked = false;
            self.verifying = false;
            changed = true;
        }
        changed
    }
}

fn judge(ctx: &Ctx<'_>, reference: &Exec, e: Exec, r: &mut Resolution) {
    if e.by == ctx.local {
        return;
    }
    if ctx.cmp.same(reference, &e) {
        r.credit.push(e.by);
    } else {
        r.ban.push(e.by);
    }
}

/// Like `judge` against a reference no coordinator recompute confirmed: agreement earns
/// credit, a difference is set aside (never a ban).
fn judge_unbanned(
    ctx: &Ctx<'_>,
    reference: &Exec,
    e: Exec,
    r: &mut Resolution,
    differ: &mut Vec<Exec>,
) {
    if e.by == ctx.local {
        return;
    }
    if ctx.cmp.same(reference, &e) {
        r.credit.push(e.by);
    } else {
        differ.push(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuee_proto::SecretKey;

    fn id(n: u8) -> EndpointId {
        SecretKey::from_bytes(&[n; 32]).public()
    }

    fn exec(by: u8, out: &[u8]) -> Exec {
        Exec {
            by: id(by),
            hash: Hash::of(out),
            output: Some(Arc::from(out)),
            ms: None,
        }
    }

    fn ctx(mode: VerifyMode, cmp: &Cmp, can_local: bool) -> Ctx<'_> {
        Ctx {
            mode,
            cmp,
            local: id(0),
            can_local,
        }
    }

    #[test]
    fn full_needs_two_agreeing_peers() {
        let cmp = Cmp::Exact;
        let c = ctx(VerifyMode::Full, &cmp, true);
        let mut t = TaskState::default();
        assert_eq!(t.want(c.mode, true), 2);
        t.results.push(exec(1, b"x"));
        t.resolve(&c, false);
        assert!(t.accepted.is_none());
        assert_eq!(t.want(c.mode, true), 1);
        t.results.push(exec(2, b"x"));
        let r = t.resolve(&c, false);
        assert!(t.verified);
        assert_eq!(r.credit.len(), 2);
    }

    #[test]
    fn full_mismatch_goes_to_local_then_bans_liar() {
        let cmp = Cmp::Exact;
        let c = ctx(VerifyMode::Full, &cmp, true);
        let mut t = TaskState::default();
        t.results.push(exec(1, b"x"));
        t.resolve(&c, false);
        t.results.push(exec(2, b"bad"));
        let r = t.resolve(&c, false);
        assert!(r.need_local && t.local_pending);
        t.results.push(exec(0, b"x"));
        let r = t.resolve(&c, false);
        assert_eq!(r.ban, vec![id(2)]);
        assert_eq!(r.credit, vec![id(1)]);
        assert!(t.verified);
    }

    #[test]
    fn full_agreement_is_sampled_by_the_coordinator_and_both_liars_banned() {
        let cmp = Cmp::Exact;
        let c = ctx(VerifyMode::Full, &cmp, true);
        let mut t = TaskState::default();
        t.results.push(exec(1, b"forged"));
        t.resolve(&c, true);
        t.results.push(exec(2, b"forged"));
        let r = t.resolve(&c, true);
        assert!(
            r.need_local && t.local_pending,
            "the pair goes to a recompute"
        );
        assert!(!t.verified && r.credit.is_empty());
        t.results.push(exec(0, b"true"));
        let mut r = t.resolve(&c, false);
        r.ban.sort();
        let mut both = vec![id(1), id(2)];
        both.sort();
        assert_eq!(r.ban, both);
        assert!(t.verified && t.accepted.as_ref().is_some_and(|a| a.by == id(0)));
    }

    #[test]
    fn mismatch_without_local_discards_both() {
        let cmp = Cmp::Exact;
        let c = ctx(VerifyMode::Sample, &cmp, false);
        let mut t = TaskState::default();
        t.results.push(exec(1, b"x"));
        t.resolve(&c, true);
        assert!(t.verifying);
        assert_eq!(t.want(c.mode, false), 1);
        t.results.push(exec(2, b"y"));
        t.resolve(&c, false);
        assert!(t.accepted.is_none() && t.require_two);
        assert_eq!(t.want(c.mode, false), 2);
    }

    #[test]
    fn purge_drops_unverified_output() {
        let cmp = Cmp::Exact;
        let c = ctx(VerifyMode::Sample, &cmp, true);
        let mut t = TaskState::default();
        t.results.push(exec(1, b"x"));
        t.resolve(&c, false);
        assert!(t.accepted.is_some());
        assert!(t.purge(&id(1)));
        assert!(t.accepted.is_none());
        assert_eq!(t.want(c.mode, true), 1);
    }
}
