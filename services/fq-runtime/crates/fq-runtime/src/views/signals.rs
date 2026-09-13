//! The operator-signal reads: what a component of the daemon said an
//! operator should look at, and how much of it there is.
//!
//! Split out of [`super`] as its own sibling, mirroring the store side
//! they read through (`control_plane::projection::store::operator_signals`).
//! The `impl Views` block below is part of the same inherent impl, so
//! the methods keep their paths; the DTOs stay with the rest of the
//! view shapes in `fq_ops::views`.

use super::{OperatorSignalDetailView, OperatorSignalView, Views, ViewsError};

impl Views {
    /// The most recent `limit` signals matching the narrowing, newest
    /// first — the pane's list.
    pub async fn operator_signals(
        &self,
        severity: Option<&str>,
        source: Option<&str>,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OperatorSignalView>, ViewsError> {
        Ok(self
            .projection
            .query_operator_signals(severity, source, since, limit)
            .await?)
    }

    /// One whole signal, with the walk through its own source's
    /// history attached. `None` when nothing is indexed under that
    /// identity.
    ///
    /// The neighbours are composed here rather than in the store
    /// because they are a property of the *answer*, not of the row: the
    /// row knows what it is, and which signals sit either side of it in
    /// the pane's order is a second question over the same table. Two
    /// extra index-covered lookups on a page an operator opens one of
    /// at a time.
    pub async fn operator_signal(
        &self,
        event_id: &str,
    ) -> Result<Option<OperatorSignalDetailView>, ViewsError> {
        let Some(mut signal) = self.projection.operator_signal(event_id).await? else {
            return Ok(None);
        };
        let (newer, older) = self
            .projection
            .operator_signal_neighbours(&signal.source, &signal.timestamp, &signal.event_id)
            .await?;
        signal.newer_from_source = newer;
        signal.older_from_source = older;
        Ok(Some(signal))
    }

    /// How many notifications landed at or after `notifications_since`,
    /// and how many alerts stand on the record — the home page's line.
    pub async fn operator_signal_counts(
        &self,
        notifications_since: Option<&str>,
    ) -> Result<fq_ops::surface::OperatorSignalCounts, ViewsError> {
        let (notifications, alerts) = self
            .projection
            .operator_signal_counts(notifications_since)
            .await?;
        Ok(fq_ops::surface::OperatorSignalCounts {
            notifications,
            alerts,
        })
    }
}
