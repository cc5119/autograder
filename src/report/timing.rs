//! What the gradebook says about *when* a submission arrived, derived from
//! the fetch record rather than the evaluation -- an `EvaluationResult`
//! carries no timestamps at all.
//!
//! Nothing here is scoring: `grade` reports lateness and leaves what to do
//! about it to the instructor, exactly as the Fetch stage does when it
//! checks a late submission out anyway.

use jiff::SignedDuration;

use crate::submissions::{Commit, FetchRecord, SubmissionDate};

/// A commit's own date can trail its push by a second or two through
/// ordinary clock skew, which isn't worth reporting as impossible.
const SKEW_TOLERANCE: SignedDuration = SignedDuration::from_secs(60);

/// How late the graded submission was.
#[derive(Debug, Clone, PartialEq)]
pub enum Lateness {
    /// On time, or deadline-exempt by design (a bless tag).
    OnTime,
    Late(SignedDuration),
    /// Nothing to claim: no fetch record, a failed fetch, a fork with no
    /// commits, or an instructor override -- an override is a deliberate
    /// exception, so reporting it as on time would launder it.
    Unknown,
}

impl Lateness {
    pub fn from_record(record: Option<&FetchRecord>) -> Self {
        let Some(record) = record else {
            return Lateness::Unknown;
        };
        match record.submission_date() {
            Some(SubmissionDate::OnTime(_) | SubmissionDate::Blessed { .. }) => Lateness::OnTime,
            Some(SubmissionDate::Late(_)) => match record.late_by() {
                Some(by) => Lateness::Late(by),
                None => Lateness::Unknown,
            },
            _ => Lateness::Unknown,
        }
    }
}

/// Which side of the deadline one timestamp falls on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Side {
    OnTime,
    Late,
}

/// Whether the graded commit's two dates agree: `push_event` is
/// server-verified and unforgeable, `commit_date` is the commit's own and
/// the student's to set.
///
/// Deliberately not a "the two differ by more than N minutes" check --
/// committing in the afternoon and pushing at midnight is ordinary, and
/// flagging it would bury the cases below in a class-sized list of
/// non-events.
#[derive(Debug, Clone, PartialEq)]
pub enum DateCheck {
    /// Both dates exist and agree about the deadline.
    Verified,
    /// No push event for this commit -- the deadline decision fell back to
    /// the forgeable commit date.
    Unverified,
    /// The two dates land on opposite sides of the deadline: one says on
    /// time and the other says late. The gate used `push_event`.
    Straddles {
        commit_date: Side,
        push_event: Side,
    },
    /// The commit is dated later than the push that carried it, beyond
    /// what clock skew explains -- a commit can't be pushed before it
    /// exists.
    CommitAfterPush(SignedDuration),
}

impl DateCheck {
    /// `None` when there's nothing to check: no record, a failed fetch, or
    /// a fork with no commits. Those are already reported as an unknown
    /// [`Lateness`], and repeating them here would say nothing new.
    pub fn from_record(record: Option<&FetchRecord>) -> Option<Self> {
        let record = record?;
        let commit = record.graded_commit()?;
        Some(Self::of(commit, record.deadline.timestamp()))
    }

    fn of(commit: &Commit, deadline: jiff::Timestamp) -> Self {
        let commit_date = commit.timestamp.commit_date;
        let Some(push_event) = commit.timestamp.push_event else {
            return DateCheck::Unverified;
        };

        if commit_date.duration_since(push_event) > SKEW_TOLERANCE {
            return DateCheck::CommitAfterPush(commit_date.duration_since(push_event));
        }

        let side = |t: jiff::Timestamp| {
            if t > deadline {
                Side::Late
            } else {
                Side::OnTime
            }
        };
        let (commit_date, push_event) = (side(commit_date), side(push_event));
        if commit_date == push_event {
            DateCheck::Verified
        } else {
            DateCheck::Straddles {
                commit_date,
                push_event,
            }
        }
    }
}
