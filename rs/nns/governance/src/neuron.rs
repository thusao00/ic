use crate::{
    governance,
    governance::{
        LOG_PREFIX, MAX_DISSOLVE_DELAY_SECONDS, MAX_NEURON_AGE_FOR_AGE_BONUS,
        MAX_NEURON_RECENT_BALLOTS, MAX_NUM_HOT_KEYS_PER_NEURON,
    },
    pb::v1::{
        audit_event::{Payload, ResetAging},
        governance_error::ErrorType,
        manage_neuron,
        neuron::DissolveState,
        AuditEvent, Ballot, BallotInfo, GovernanceError, Neuron, NeuronInfo, NeuronState, Topic,
        Vote,
    },
};
use dfn_core::println;
use ic_base_types::PrincipalId;
use ic_nns_common::pb::v1::{NeuronId, ProposalId};
use std::collections::HashMap;

// Use the same logic as GTC canister for resetting the aging timestamp.
const ONE_DAY_SECONDS: u64 = 24 * 60 * 60;
const ONE_YEAR_SECONDS: u64 = (4 * 365 + 1) * ONE_DAY_SECONDS / 4;
const ONE_MONTH_SECONDS: u64 = ONE_YEAR_SECONDS / 12;
const GTC_NEURON_PRE_AGE_DURATION_SECONDS: u64 = 18 * ONE_MONTH_SECONDS;

impl Neuron {
    // --- Utility methods on neurons: mostly not for public consumption.

    /// Returns the state the neuron would be in a time
    /// `now_seconds`. See [NeuronState] for details.
    pub fn state(&self, now_seconds: u64) -> NeuronState {
        if self.spawn_at_timestamp_seconds.is_some() {
            return NeuronState::Spawning;
        }
        match self.dissolve_state {
            Some(DissolveState::DissolveDelaySeconds(d)) => {
                if d > 0 {
                    NeuronState::NotDissolving
                } else {
                    NeuronState::Dissolved
                }
            }
            Some(DissolveState::WhenDissolvedTimestampSeconds(ts)) => {
                if ts > now_seconds {
                    NeuronState::Dissolving
                } else {
                    NeuronState::Dissolved
                }
            }
            None => NeuronState::Dissolved,
        }
    }

    /// Returns true if and only if `principal` is equal to the
    /// controller of this neuron.
    pub(crate) fn is_controlled_by(&self, principal: &PrincipalId) -> bool {
        self.controller.as_ref().map_or(false, |c| c == principal)
    }

    /// Returns true if and only if `principal` is authorized to
    /// perform non-privileged operations, like vote and follow,
    /// on behalf of this neuron, i.e., if `principal` is either the
    /// controller or one of the authorized hot keys.
    pub(crate) fn is_authorized_to_vote(&self, principal: &PrincipalId) -> bool {
        self.is_hotkey_or_controller(principal)
    }

    /// Returns true if and only if `principal` is authorized to
    /// call simulate_manage_neuron requests on this neuron
    pub(crate) fn is_authorized_to_simulate_manage_neuron(&self, principal: &PrincipalId) -> bool {
        self.is_hotkey_or_controller(principal)
    }

    /// Returns true if and only if `principal` is either the controller or a hotkey
    fn is_hotkey_or_controller(&self, principal: &PrincipalId) -> bool {
        self.is_controlled_by(principal) || self.hot_keys.contains(principal)
    }

    /// Returns true if this is a community fund neuron.
    pub(crate) fn is_community_fund_neuron(&self) -> bool {
        self.joined_community_fund_timestamp_seconds.is_some()
    }

    /// Return the voting power of this neuron.
    ///
    /// The voting power is the stake of the neuron modified by a
    /// bonus of up to 100% depending on the dissolve delay, with
    /// the maximum bonus of 100% received at an 8 year dissolve
    /// delay. The voting power is further modified by the age of
    /// the neuron giving up to 25% bonus after four years.
    pub fn voting_power(&self, now_seconds: u64) -> u64 {
        // We compute the stake adjustments in u128.
        let stake = self.stake_e8s() as u128;
        // Dissolve delay is capped to eight years, but we cap it
        // again here to make sure, e.g., if this changes in the
        // future.
        let d = std::cmp::min(
            self.dissolve_delay_seconds(now_seconds),
            MAX_DISSOLVE_DELAY_SECONDS,
        ) as u128;
        // 'd_stake' is the stake with bonus for dissolve delay.
        let d_stake = stake + ((stake * d) / (MAX_DISSOLVE_DELAY_SECONDS as u128));
        // Sanity check.
        assert!(d_stake <= 2 * stake);
        // The voting power is also a function of the age of the
        // neuron, giving a bonus of up to 25% at the four year mark.
        let a = std::cmp::min(self.age_seconds(now_seconds), MAX_NEURON_AGE_FOR_AGE_BONUS) as u128;
        let ad_stake = d_stake + ((d_stake * a) / (4 * MAX_NEURON_AGE_FOR_AGE_BONUS as u128));
        // Final stake 'ad_stake' is at most 5/4 of the 'd_stake'.
        assert!(ad_stake <= (5 * d_stake) / 4);
        // The final voting power is the stake adjusted by both age
        // and dissolve delay. If the stake is is greater than
        // u64::MAX divided by 2.5, the voting power may actually not
        // fit in a u64.
        std::cmp::min(ad_stake, u64::MAX as u128) as u64
    }

    /// Given the specified `ballots`: determine how this neuron would
    /// vote on a proposal of `topic` based on which neurons this
    /// neuron follows on this topic (or on the default topic if this
    /// neuron doesn't specify any followees for `topic`).
    pub(crate) fn would_follow_ballots(
        &self,
        topic: Topic,
        ballots: &HashMap<u64, Ballot>,
    ) -> Vote {
        // Compute the list of followees for this topic. If no
        // following is specified for the topic, use the followees
        // from the 'Unspecified' topic.
        if let Some(followees) = self
            .followees
            .get(&(topic as i32))
            .or_else(|| self.followees.get(&(Topic::Unspecified as i32)))
            // extract plain vector from 'Followees' proto
            .map(|x| &x.followees)
        {
            // If, for some reason, a list of followees is specified
            // but empty (this is not normal), don't vote 'no', as
            // would be the natural result of the algorithm below, but
            // instead don't cast a vote.
            if followees.is_empty() {
                return Vote::Unspecified;
            }
            let mut yes: usize = 0;
            let mut no: usize = 0;
            for f in followees.iter() {
                if let Some(f_vote) = ballots.get(&f.id) {
                    if f_vote.vote == (Vote::Yes as i32) {
                        yes += 1;
                    } else if f_vote.vote == (Vote::No as i32) {
                        no += 1;
                    }
                }
            }
            if 2 * yes > followees.len() {
                return Vote::Yes;
            }
            if 2 * no >= followees.len() {
                return Vote::No;
            }
        }
        // No followees specified.
        Vote::Unspecified
    }

    /// Returns the list of followees on the manage neuron topic for
    /// this neuron.
    pub(crate) fn neuron_managers(&self) -> Option<&Vec<NeuronId>> {
        self.followees
            .get(&(Topic::NeuronManagement as i32))
            .map(|x| &x.followees)
    }

    /// Register that this neuron has cast a ballot for a
    /// proposal. Don't include votes on "real time" topics (such as
    /// setting the ICP/SDR exchange rate).
    pub(crate) fn register_recent_ballot(
        &mut self,
        topic: Topic,
        proposal_id: &ProposalId,
        vote: Vote,
    ) {
        // Ignore votes on topics for which no public voting history
        // is required.
        if topic == Topic::ExchangeRate {
            return;
        }
        let ballot_info = BallotInfo {
            proposal_id: Some(*proposal_id),
            vote: vote as i32,
        };
        // We would really like to have a circular buffer here. As
        // we're dealing with a simple vector, we insert at the
        // beginning and remove at the end once we have reached
        // the maximum number of votes to keep track of.
        self.recent_ballots.insert(0, ballot_info);
        // Pop and discard elements from the end until we reach
        // the maximum allowed length of the vector.
        while self.recent_ballots.len() > MAX_NEURON_RECENT_BALLOTS {
            self.recent_ballots.pop();
        }
    }

    // See the relevant protobuf for a high-level description of
    // these operations

    /// If this method is called on a non-dissolving neuron, it remains
    /// non-dissolving. If it is called on dissolving neuron, it remains
    /// dissolving.
    ///
    /// If it is called on a dissolved neuron, it becomes non-dissolving and
    /// its 'age' is reset to start counting from when it last entered
    /// the dissolved state, when applicable (that is, the Dissolved state
    /// was reached through explicit dissolution) --- or from `now` when not
    /// applicable (e.g., newly created neuron with zero dissolve delay).
    pub(crate) fn increase_dissolve_delay(
        &mut self,
        now_seconds: u64,
        additional_dissolve_delay_seconds: u32,
    ) {
        let additional_delay = additional_dissolve_delay_seconds as u64;
        // If there is no dissolve delay, this is a no-op.  Upstream validation can decide if
        // an error should be returned to the user.
        if additional_delay == 0 {
            return;
        }
        match self.dissolve_state {
            Some(DissolveState::DissolveDelaySeconds(delay)) => {
                let new_delay = std::cmp::min(
                    delay.saturating_add(additional_delay),
                    MAX_DISSOLVE_DELAY_SECONDS,
                );
                // Note that if delay == 0, this neuron was
                // dissolved and it now becomes non-dissolving.
                self.dissolve_state = Some(DissolveState::DissolveDelaySeconds(new_delay));
                if delay == 0 {
                    // We transition from `Dissolved` to `NotDissolving`: reset age.
                    self.aging_since_timestamp_seconds = now_seconds;
                }
            }
            Some(DissolveState::WhenDissolvedTimestampSeconds(ts)) => {
                if ts > now_seconds {
                    let delay = ts - now_seconds;
                    let new_delay = std::cmp::min(
                        delay.saturating_add(additional_delay),
                        MAX_DISSOLVE_DELAY_SECONDS,
                    );
                    let new_ts = now_seconds + new_delay;
                    // Sanity check:
                    // if additional_delay == 0, then
                    // new_delay == delay == ts - now_seconds, whence
                    // new_ts == now_seconds + ts - now_seconds == ts
                    self.dissolve_state =
                        Some(DissolveState::WhenDissolvedTimestampSeconds(new_ts));
                    // The neuron was and remains `Dissolving`:
                    // its effective neuron age should already be
                    // zero by having an `aging_since` timestamp
                    // in the far future. Reset it just in case.
                    self.aging_since_timestamp_seconds = u64::MAX;
                } else {
                    // ts <= now_seconds
                    // This neuron is dissolved. Set it to non-dissolving.
                    let new_delay = std::cmp::min(additional_delay, MAX_DISSOLVE_DELAY_SECONDS);
                    self.dissolve_state = Some(DissolveState::DissolveDelaySeconds(new_delay));
                    // We transition from `Dissolved` to `NotDissolving`: reset age.
                    self.aging_since_timestamp_seconds = now_seconds;
                }
            }
            None => {
                // This neuron is dissolved. Set it to non-dissolving.
                let new_delay = std::cmp::min(additional_delay, MAX_DISSOLVE_DELAY_SECONDS);
                self.dissolve_state = Some(DissolveState::DissolveDelaySeconds(new_delay));
                // We transition from `Dissolved` to `NotDissolving`: reset age.
                self.aging_since_timestamp_seconds = now_seconds;
            }
        }
    }

    /// Join the Internet Computer's community fund. If this neuron is
    /// already a member of the community fund, an error is returned.
    fn join_community_fund(&mut self, now_seconds: u64) -> Result<(), GovernanceError> {
        if self.joined_community_fund_timestamp_seconds.unwrap_or(0) == 0 {
            self.joined_community_fund_timestamp_seconds = Some(now_seconds);
            Ok(())
        } else {
            // Already joined...
            Err(GovernanceError::new(ErrorType::AlreadyJoinedCommunityFund))
        }
    }

    /// Leave the Internet Computer's community fund. If this neuron is not a
    /// member of the community fund, an error will be returned.
    fn leave_community_fund(&mut self) -> Result<(), GovernanceError> {
        if self.joined_community_fund_timestamp_seconds.unwrap_or(0) != 0 {
            self.joined_community_fund_timestamp_seconds = None;
            Ok(())
        } else {
            Err(GovernanceError::new(ErrorType::NotInTheCommunityFund))
        }
    }

    /// If this neuron is not dissolving, start dissolving it.
    ///
    /// If the neuron is dissolving or dissolved, an error is returned.
    fn start_dissolving(&mut self, now_seconds: u64) -> Result<(), GovernanceError> {
        if let Some(DissolveState::DissolveDelaySeconds(delay)) = self.dissolve_state {
            // Neuron is actually not dissolving.
            if delay > 0 {
                self.dissolve_state = Some(DissolveState::WhenDissolvedTimestampSeconds(
                    delay + now_seconds,
                ));
                // When we start dissolving, we set the neuron age to
                // zero, and it stays zero until we stop
                // dissolving. This is represented by setting the
                // 'aging since' to its maximum possible value, which
                // will remain in the future until approximately
                // 292,277,026,596 AD.
                self.aging_since_timestamp_seconds = u64::MAX;
                Ok(())
            } else {
                // Already dissolved - cannot start dissolving.
                Err(GovernanceError::new(ErrorType::RequiresNotDissolving))
            }
        } else {
            // Already dissolving or dissolved - cannot start dissolving.
            Err(GovernanceError::new(ErrorType::RequiresNotDissolving))
        }
    }

    /// If this neuron is dissolving, set it to not dissolving.
    ///
    /// If the neuron is not dissolving, an error is returned.
    fn stop_dissolving(&mut self, now_seconds: u64) -> Result<(), GovernanceError> {
        if let Some(DissolveState::WhenDissolvedTimestampSeconds(ts)) = self.dissolve_state {
            if ts > now_seconds {
                // Dissolve time is in the future: pause dissolving.
                self.dissolve_state = Some(DissolveState::DissolveDelaySeconds(ts - now_seconds));
                self.aging_since_timestamp_seconds = now_seconds;
                Ok(())
            } else {
                // Neuron is already dissolved, so it doesn't
                // make sense to stop dissolving it.
                Err(GovernanceError::new(ErrorType::RequiresDissolving))
            }
        } else {
            // The neuron is not in a dissolving state.
            Err(GovernanceError::new(ErrorType::RequiresDissolving))
        }
    }

    /// Preconditions:
    /// - key to add is not already present in 'hot_keys'
    /// - the key to add is well-formed
    /// - there are not already too many hot keys for this neuron.
    fn add_hot_key(&mut self, new_hot_key: &PrincipalId) -> Result<(), GovernanceError> {
        // Make sure that the same hot key is not added twice.
        for key in &self.hot_keys {
            if *key == *new_hot_key {
                return Err(GovernanceError::new_with_message(
                    ErrorType::HotKey,
                    "Hot key duplicated.",
                ));
            }
        }
        // Allow at most 10 hot keys per neuron.
        if self.hot_keys.len() >= MAX_NUM_HOT_KEYS_PER_NEURON {
            return Err(GovernanceError::new_with_message(
                ErrorType::ResourceExhausted,
                "Reached the maximum number of hotkeys.",
            ));
        }
        self.hot_keys.push(*new_hot_key);
        Ok(())
    }

    /// Precondition: key to remove is present in 'hot_keys'
    fn remove_hot_key(&mut self, hot_key_to_remove: &PrincipalId) -> Result<(), GovernanceError> {
        if let Some(index) = self.hot_keys.iter().position(|x| *x == *hot_key_to_remove) {
            self.hot_keys.swap_remove(index);
            Ok(())
        } else {
            // Hot key to remove was not found.
            Err(GovernanceError::new_with_message(
                ErrorType::NotFound,
                "Remove failed: Hot key not found.",
            ))
        }
    }

    // --- Public interface of a neuron.

    /// Return the age of this neuron.
    ///
    /// A dissolving neuron has age zero.
    ///
    /// Technically, each neuron has an internal `aging_since`
    /// field that is set to the current time when a neuron is
    /// created in a non-dissolving state and reset when a neuron is
    /// not dissolving again after a call to `stop_dissolve`. While a
    /// neuron is dissolving, `aging_since` is a value in the far
    /// future, effectively making its age zero.
    pub fn age_seconds(&self, now_seconds: u64) -> u64 {
        now_seconds.saturating_sub(self.aging_since_timestamp_seconds)
    }

    /// Returns the dissolve delay of this neuron. For a non-dissolving
    /// neuron, this is just the recorded dissolve delay; for a
    /// dissolving neuron, this is the the time left (from
    /// `now_seconds`) until the neuron becomes dissolved; for a
    /// dissolved neuron, this function returns zero.
    pub fn dissolve_delay_seconds(&self, now_seconds: u64) -> u64 {
        match self.dissolve_state {
            Some(DissolveState::DissolveDelaySeconds(d)) => d,
            Some(DissolveState::WhenDissolvedTimestampSeconds(ts)) => {
                ts.saturating_sub(now_seconds)
            }
            None => 0,
        }
    }

    pub fn is_dissolved(&self, now_seconds: u64) -> bool {
        self.dissolve_delay_seconds(now_seconds) == 0
    }

    fn is_authorized_to_configure_or_err(
        &self,
        caller: &PrincipalId,
        configure: &manage_neuron::configure::Operation,
    ) -> Result<(), GovernanceError> {
        use manage_neuron::configure::Operation::{JoinCommunityFund, LeaveCommunityFund};

        match configure {
            // The controller and hotkeys are allowed to change Neuron Fund membership.
            JoinCommunityFund(_) | LeaveCommunityFund(_) => {
                if self.is_hotkey_or_controller(caller) {
                    Ok(())
                } else {
                    Err(GovernanceError::new_with_message(
                        ErrorType::NotAuthorized,
                        format!(
                            "Caller '{:?}' must be the controller or hotkey of the neuron to join or leave the neuron fund.",
                            caller,
                        ),
                    ))
                }
            }

            // Only the controller is allowed to perform other configure operations.
            _ => {
                if self.is_controlled_by(caller) {
                    Ok(())
                } else {
                    Err(GovernanceError::new_with_message(
                        ErrorType::NotAuthorized,
                        format!(
                            "Caller '{:?}' must be the controller of the neuron to perform this operation:\n{:#?}",
                            caller,
                            configure,
                        ),
                    ))
                }
            }
        }
    }

    /// Apply the specified neuron configuration operation on this neuron.
    ///
    /// See [manage_neuron::Configure] for details.
    pub fn configure(
        &mut self,
        caller: &PrincipalId,
        now_seconds: u64,
        cmd: &manage_neuron::Configure,
    ) -> Result<(), GovernanceError> {
        let op = &cmd.operation.as_ref().ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::InvalidCommand,
                "Configure must have an operation.",
            )
        })?;

        self.is_authorized_to_configure_or_err(caller, op)?;

        match op {
            manage_neuron::configure::Operation::IncreaseDissolveDelay(d) => {
                if d.additional_dissolve_delay_seconds == 0 {
                    return Err(GovernanceError::new_with_message(
                        ErrorType::InvalidCommand,
                        "Additional delay is 0.",
                    ));
                }
                self.increase_dissolve_delay(now_seconds, d.additional_dissolve_delay_seconds);
                Ok(())
            }
            manage_neuron::configure::Operation::SetDissolveTimestamp(d) => {
                if now_seconds > d.dissolve_timestamp_seconds {
                    return Err(GovernanceError::new_with_message(
                        ErrorType::InvalidCommand,
                        "The dissolve delay must be set to a future time.",
                    ));
                }
                let desired_dd = d.dissolve_timestamp_seconds - now_seconds;
                let current_dd = self.dissolve_delay_seconds(now_seconds);

                if current_dd > desired_dd {
                    return Err(GovernanceError::new_with_message(
                        ErrorType::InvalidCommand,
                        "Can't set a dissolve delay that is smaller than the current dissolve delay."
                    ));
                }

                let dd_diff = desired_dd - current_dd;
                if dd_diff == 0 {
                    return Err(GovernanceError::new_with_message(
                        ErrorType::InvalidCommand,
                        "Additional delay is 0.",
                    ));
                }
                self.increase_dissolve_delay(
                    now_seconds,
                    dd_diff.try_into().map_err(|_| {
                        GovernanceError::new_with_message(
                            ErrorType::InvalidCommand,
                            "Can't convert u64 dissolve delay into u32.",
                        )
                    })?,
                );
                Ok(())
            }
            manage_neuron::configure::Operation::StartDissolving(_) => {
                self.start_dissolving(now_seconds)
            }
            manage_neuron::configure::Operation::StopDissolving(_) => {
                self.stop_dissolving(now_seconds)
            }
            manage_neuron::configure::Operation::AddHotKey(k) => {
                let hot_key = k.new_hot_key.as_ref().ok_or_else(|| {
                    GovernanceError::new_with_message(
                    ErrorType::InvalidCommand,
                    "Operation AddHotKey requires the hot key to add to be specified in the input",
                )
                })?;
                self.add_hot_key(hot_key)
            }
            manage_neuron::configure::Operation::RemoveHotKey(k) => {
                let hot_key = k.hot_key_to_remove.as_ref().ok_or_else(|| GovernanceError::new_with_message(
                    ErrorType::InvalidCommand,
                    "Operation RemoveHotKey requires the hot key to remove to be specified in the input",
                ))?;
                self.remove_hot_key(hot_key)
            }
            manage_neuron::configure::Operation::JoinCommunityFund(_) => {
                self.join_community_fund(now_seconds)
            }
            manage_neuron::configure::Operation::LeaveCommunityFund(_) => {
                self.leave_community_fund()
            }
            manage_neuron::configure::Operation::ChangeAutoStakeMaturity(change) => {
                if change.requested_setting_for_auto_stake_maturity {
                    self.auto_stake_maturity = Some(true);
                } else {
                    self.auto_stake_maturity = None;
                }
                Ok(())
            }
        }
    }

    /// Get the 'public' information associated with this neuron.
    pub fn get_neuron_info(&self, now_seconds: u64) -> NeuronInfo {
        NeuronInfo {
            retrieved_at_timestamp_seconds: now_seconds,
            state: self.state(now_seconds) as i32,
            age_seconds: self.age_seconds(now_seconds),
            dissolve_delay_seconds: self.dissolve_delay_seconds(now_seconds),
            recent_ballots: self.recent_ballots.clone(),
            voting_power: self.voting_power(now_seconds),
            created_timestamp_seconds: self.created_timestamp_seconds,
            stake_e8s: self.minted_stake_e8s(),
            joined_community_fund_timestamp_seconds: self.joined_community_fund_timestamp_seconds,
            known_neuron_data: self.known_neuron_data.as_ref().cloned(),
        }
    }

    /// Return the current 'stake' of this Neuron in number of 10^-8 ICPs.
    /// (That is, if the stake is 1 ICP, this function will return 10^8).
    ///
    /// The stake can be decreased by making proposals that are
    /// subsequently rejected, and increased by transferring funds
    /// to the account of this neuron and then refreshing the stake, or
    /// by accumulating staked maturity.
    pub fn stake_e8s(&self) -> u64 {
        self.cached_neuron_stake_e8s
            .saturating_sub(self.neuron_fees_e8s)
            .saturating_add(self.staked_maturity_e8s_equivalent.unwrap_or(0))
    }

    /// Returns the current `minted` stake of the neuron, i.e. the ICP backing the
    /// neuron, minus the fees. This does not count staked maturity.
    pub fn minted_stake_e8s(&self) -> u64 {
        self.cached_neuron_stake_e8s
            .saturating_sub(self.neuron_fees_e8s)
    }

    /// Set the cached stake of this neuron to `updated_stake_e8s` and adjust
    /// this neuron's age to be the weighted average of the priorly cached
    /// and the added stakes. For example, if neuron N had staked 10 ICP aging
    /// since 3 years and 5 ICP has been added, then
    /// `N.update_stake_adjust_age(15 ICP)` will result in N staking 15 ICP aged
    /// at (10 ICP * 3 years) / (10 ICP + 5 ICP) = 2 years.
    ///
    /// Only a non-dissolving neuron has a non-zero age. The age of all other
    /// neurons (i.e., dissolving and dissolved) is represented as
    /// `againg_since_timestamp_seconds == u64::MAX`. This method maintains
    /// that invariant.
    pub fn update_stake_adjust_age(&mut self, updated_stake_e8s: u64, now: u64) {
        // If the updated stake is less than the original stake, preserve the
        // age and distribute it over the new amount. This should not happen
        // in practice, so this code exists merely as a defensive fallback.
        //
        // TODO(NNS1-954) Consider whether update_stake_adjust_age (and other
        // similar methods) should use a neurons effective stake rather than
        // the cached stake.
        if updated_stake_e8s < self.cached_neuron_stake_e8s {
            println!(
                "{}Reducing neuron {:?} stake via update_stake_adjust_age: {} -> {}",
                LOG_PREFIX, self.id, self.cached_neuron_stake_e8s, updated_stake_e8s
            );
            self.cached_neuron_stake_e8s = updated_stake_e8s;
        } else {
            // If one looks at "stake * age" as describing an area, the goal
            // at this point is to increase the stake while keeping the area
            // constant. This means decreasing the age in proportion to the
            // additional stake, which is the purpose of combine_aged_stakes.
            let (new_stake_e8s, new_age_seconds) = governance::combine_aged_stakes(
                self.cached_neuron_stake_e8s,
                self.age_seconds(now),
                updated_stake_e8s.saturating_sub(self.cached_neuron_stake_e8s),
                0,
            );
            // A consequence of the math above is that the 'new_stake_e8s' is
            // always the same as the 'updated_stake_e8s'. We use
            // 'combine_aged_stakes' here to make sure the age is
            // appropriately pro-rated to accommodate the new stake.
            assert!(new_stake_e8s == updated_stake_e8s);
            self.cached_neuron_stake_e8s = new_stake_e8s;

            self.aging_since_timestamp_seconds =
                if let Some(DissolveState::WhenDissolvedTimestampSeconds(_)) = self.dissolve_state {
                    // Check if invariant is violated.
                    if self.aging_since_timestamp_seconds != u64::MAX {
                        println!(
                            "{}Neuron {:?} is in state {:?}, so it should not have \
                         an age, but aging_since_timestamp_seconds = {}",
                            LOG_PREFIX,
                            self.id,
                            self.state(now),
                            self.aging_since_timestamp_seconds
                        );
                    }
                    // If, for some reason, the invariant did not already hold, we
                    // recover by re-establishing it.
                    u64::MAX
                } else {
                    // Only a non-dissolving neurons have a non-zero age.
                    now.saturating_sub(new_age_seconds)
                }
        }
    }

    /// If the aging timestamp is earlier than GENESIS - PRE_AGE, reset it to GENISIS.
    pub fn maybe_reset_aging_timestamp(&mut self, now: u64) -> Option<AuditEvent> {
        let genesis_timestamp_seconds = ic_types::time::GENESIS.as_secs_since_unix_epoch();
        let aging_limit_timestamp_seconds =
            genesis_timestamp_seconds.saturating_sub(GTC_NEURON_PRE_AGE_DURATION_SECONDS);
        let should_reset = self.aging_since_timestamp_seconds < aging_limit_timestamp_seconds;
        if should_reset {
            let event = AuditEvent {
                timestamp_seconds: now,
                payload: Some(Payload::ResetAging(ResetAging {
                    neuron_id: self.id.as_ref().map(|id| id.id).unwrap_or_default(),
                    previous_aging_since_timestamp_seconds: self.aging_since_timestamp_seconds,
                    new_aging_since_timestamp_seconds: genesis_timestamp_seconds,
                    neuron_dissolve_state: self
                        .dissolve_state
                        .clone()
                        .map(|dissolve_state| dissolve_state.into()),
                    neuron_stake_e8s: self.minted_stake_e8s(),
                })),
            };
            self.aging_since_timestamp_seconds = genesis_timestamp_seconds;
            Some(event)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::pb::v1::{
        audit_event::{Payload, ResetAging},
        neuron::DissolveState,
        AuditEvent, Neuron, NeuronState,
    };
    use ic_nervous_system_common::{E8, SECONDS_PER_DAY};

    const NOW: u64 = 123_456_789;

    #[test]
    fn reset_aging_timestamp_should_reset() {
        let mut neuron = Neuron {
            aging_since_timestamp_seconds: 1_572_992_229, // Tue, 05 Nov 2019 22:17:09 GMT
            ..Default::default()
        };

        assert_eq!(
            neuron.maybe_reset_aging_timestamp(NOW),
            Some(AuditEvent {
                timestamp_seconds: 123_456_789,
                payload: Some(Payload::ResetAging(ResetAging {
                    neuron_id: 0,
                    previous_aging_since_timestamp_seconds: 1_572_992_229,
                    new_aging_since_timestamp_seconds: 1_620_328_630,
                    neuron_dissolve_state: neuron.dissolve_state.clone().map(|state| state.into()),
                    neuron_stake_e8s: neuron.minted_stake_e8s(),
                }))
            })
        );

        assert_eq!(
            neuron.aging_since_timestamp_seconds,
            1_620_328_630 // Thu, 06 May 2021 19:17:10 GMT (Genesis)
        );
    }

    #[test]
    fn reset_aging_timestamp_no_op() {
        let mut neuron = Neuron {
            aging_since_timestamp_seconds: 1_572_992_230, // Tue, 05 Nov 2019 22:17:10 GMT
            ..Default::default()
        };

        assert!(neuron.maybe_reset_aging_timestamp(NOW).is_none());

        assert_eq!(neuron.aging_since_timestamp_seconds, 1_572_992_230);
    }

    const TWELVE_MONTHS_SECONDS: u64 = 30 * 12 * 24 * 60 * 60;

    #[test]
    fn test_update_stake_adjust_age_for_dissolved_neuron_variant_a_now() {
        // WhenDissolvedTimestampSeconds(NOW) ==> dissolved
        let original_dissolve_state = Some(DissolveState::WhenDissolvedTimestampSeconds(NOW));
        let mut neuron = Neuron {
            aging_since_timestamp_seconds: u64::MAX,
            cached_neuron_stake_e8s: 10 * E8,
            dissolve_state: original_dissolve_state.clone(),
            ..Default::default()
        };
        let new_stake_e8s = 1_500_000_000_u64; // 15 ICP
        neuron.update_stake_adjust_age(new_stake_e8s, NOW);
        assert_eq!(neuron.dissolve_state, original_dissolve_state);
        assert_eq!(neuron.cached_neuron_stake_e8s, new_stake_e8s);
        assert_eq!(neuron.aging_since_timestamp_seconds, u64::MAX);
    }

    #[test]
    fn test_update_stake_adjust_age_for_dissolved_neuron_variant_a_past() {
        // WhenDissolvedTimestampSeconds(past) ==> dissolved
        let original_dissolve_state = Some(DissolveState::WhenDissolvedTimestampSeconds(
            NOW.saturating_sub(TWELVE_MONTHS_SECONDS),
        ));
        let mut neuron = Neuron {
            aging_since_timestamp_seconds: u64::MAX,
            cached_neuron_stake_e8s: 10 * E8,
            dissolve_state: original_dissolve_state.clone(),
            ..Default::default()
        };
        let new_stake_e8s = 1_500_000_000_u64; // 15 ICP
        neuron.update_stake_adjust_age(new_stake_e8s, NOW);
        assert_eq!(neuron.dissolve_state, original_dissolve_state);
        assert_eq!(neuron.cached_neuron_stake_e8s, new_stake_e8s);
        assert_eq!(neuron.aging_since_timestamp_seconds, u64::MAX);
    }

    #[test]
    fn test_update_stake_adjust_age_for_dissolved_neuron_variant_b() {
        let original_dissolve_state = Some(DissolveState::DissolveDelaySeconds(0));
        let mut neuron = Neuron {
            aging_since_timestamp_seconds: NOW.saturating_sub(TWELVE_MONTHS_SECONDS),
            cached_neuron_stake_e8s: 10 * E8,
            dissolve_state: original_dissolve_state.clone(),
            ..Default::default()
        };
        let new_stake_e8s: u64 = 1_500_000_000_u64; // 15 ICP
        neuron.update_stake_adjust_age(new_stake_e8s, NOW);
        assert_eq!(neuron.dissolve_state, original_dissolve_state);
        assert_eq!(neuron.cached_neuron_stake_e8s, new_stake_e8s);
        // This is the weighted average that tells us what the age should be
        // in seconds.
        let expected_new_age_seconds = TWELVE_MONTHS_SECONDS.saturating_mul(10).saturating_div(15);
        // Decrease the age that we expect from now to get the exected timestamp
        // since when the neurons should be aging.
        assert_eq!(neuron.age_seconds(NOW), expected_new_age_seconds);
    }

    #[test]
    fn test_update_stake_adjust_age_for_dissolved_neuron_variant_c() {
        // This should mean the neuron is dissolved.
        let original_dissolve_state = None;
        let mut neuron = Neuron {
            aging_since_timestamp_seconds: NOW.saturating_sub(TWELVE_MONTHS_SECONDS),
            cached_neuron_stake_e8s: 10 * E8,
            dissolve_state: original_dissolve_state.clone(),
            ..Default::default()
        };
        let new_stake_e8s = 1_500_000_000_u64; // 15 ICP
        neuron.update_stake_adjust_age(new_stake_e8s, NOW);
        assert_eq!(neuron.dissolve_state, original_dissolve_state);
        assert_eq!(neuron.cached_neuron_stake_e8s, new_stake_e8s);
        // This is the weighted average that tells us what the age should be
        // in seconds.
        let expected_new_age_seconds = TWELVE_MONTHS_SECONDS.saturating_mul(10).saturating_div(15);
        // Decrease the age that we expect from now to get the exected timestamp
        // since when the neurons should be aging.
        assert_eq!(neuron.age_seconds(NOW), expected_new_age_seconds);
    }

    #[test]
    fn test_update_stake_adjust_age_for_non_dissolving_neuron() {
        let original_dissolve_state =
            Some(DissolveState::DissolveDelaySeconds(TWELVE_MONTHS_SECONDS));
        let mut neuron = Neuron {
            aging_since_timestamp_seconds: NOW.saturating_sub(TWELVE_MONTHS_SECONDS),
            cached_neuron_stake_e8s: 10 * E8,
            dissolve_state: original_dissolve_state.clone(),
            ..Default::default()
        };
        let new_stake_e8s = 1_500_000_000_u64; // 15 ICP
        neuron.update_stake_adjust_age(new_stake_e8s, NOW);
        assert_eq!(neuron.dissolve_state, original_dissolve_state);
        // This is the weighted average that tells us what the age should be
        // in seconds.
        let expected_new_age_seconds = TWELVE_MONTHS_SECONDS.saturating_mul(10).saturating_div(15);
        // Decrease the age that we expect from now to get the exected timestamp
        // since when the neurons should be aging.
        assert_eq!(neuron.age_seconds(NOW), expected_new_age_seconds);
    }

    #[test]
    fn test_update_stake_adjust_age_for_dissolving_neuron() {
        // WhenDissolvedTimestampSeconds(future) <==> dissolving
        let original_dissolve_state = Some(DissolveState::WhenDissolvedTimestampSeconds(
            NOW.saturating_add(TWELVE_MONTHS_SECONDS),
        ));
        let mut neuron = Neuron {
            aging_since_timestamp_seconds: u64::MAX,
            cached_neuron_stake_e8s: 10 * E8,
            dissolve_state: original_dissolve_state.clone(),
            ..Default::default()
        };
        let new_stake_e8s = 15 * E8;
        neuron.update_stake_adjust_age(new_stake_e8s, NOW);
        assert_eq!(neuron.dissolve_state, original_dissolve_state);
        assert_eq!(neuron.cached_neuron_stake_e8s, new_stake_e8s);
        assert_eq!(neuron.aging_since_timestamp_seconds, u64::MAX);
    }

    #[test]
    fn test_update_stake_adjust_age_for_invalid_cache() {
        // For a neuron N, the value of the `N.cached_neuron_stake_e8s` should
        // monotonically grow over time. If this invariant is violated, that
        // means the cache was invalid. Calling `N.update_stake_adjust_age(X)`
        // should recover an invalid cache by setting it to `X`.
        let mut neuron = Neuron {
            cached_neuron_stake_e8s: 10 * E8,
            ..Default::default()
        };
        let original_dissolve_state = neuron.dissolve_state.clone();
        // We expect that the age does not change in this scenario.
        let original_aging_since_timestamp_seconds = neuron.aging_since_timestamp_seconds;
        let new_stake_e8s = 5 * E8;
        neuron.update_stake_adjust_age(new_stake_e8s, NOW);
        // The only effect of the above call shoudl be an update of
        // `cached_neuron_stake_e8s`; e.g., the operation does not simply fail.
        assert_eq!(neuron.dissolve_state, original_dissolve_state);
        assert_eq!(neuron.cached_neuron_stake_e8s, new_stake_e8s);
        assert_eq!(
            neuron.aging_since_timestamp_seconds,
            original_aging_since_timestamp_seconds
        );
    }

    #[test]
    fn increase_dissolve_delay_sets_age_correctly_for_dissolved_neurons() {
        // We set NOW to const in the test since it's shared in the cases and the test impl fn
        const NOW: u64 = 1000;
        fn test_increase_dissolve_delay_by_1_on_dissolved_neuron(
            current_aging_since_timestamp_seconds: u64,
            current_dissolve_state: Option<DissolveState>,
        ) {
            let mut neuron = Neuron {
                aging_since_timestamp_seconds: current_aging_since_timestamp_seconds,
                dissolve_state: current_dissolve_state,
                ..Default::default()
            };
            // precondition, neuron is considered dissolved
            assert_eq!(neuron.state(NOW), NeuronState::Dissolved);

            neuron.increase_dissolve_delay(NOW, 1);

            // Post-condition - always aging_since_timestamp_seconds = now
            // always DissolveState::DissolveDelaySeconds(1)
            assert_eq!(
                neuron,
                Neuron {
                    aging_since_timestamp_seconds: NOW,
                    dissolve_state: Some(DissolveState::DissolveDelaySeconds(1)),
                    ..Default::default()
                }
            );
        }

        #[rustfmt::skip]
        let cases = [
            // These invalid cases ensure that the method actually transforms "now" correctly
            (0, Some(DissolveState::DissolveDelaySeconds(0))),
            (0, Some(DissolveState::WhenDissolvedTimestampSeconds(NOW))),
            (0, Some(DissolveState::WhenDissolvedTimestampSeconds(NOW - 1))),
            (0, Some(DissolveState::WhenDissolvedTimestampSeconds(0))),
            (0, None),
            // These are also inconsistent with what should be observed.
            (NOW + 100, Some(DissolveState::DissolveDelaySeconds(0))),
            (NOW + 100, Some(DissolveState::WhenDissolvedTimestampSeconds(NOW))),
            (NOW + 100, Some(DissolveState::WhenDissolvedTimestampSeconds(NOW - 1))),
            (NOW + 100, Some(DissolveState::WhenDissolvedTimestampSeconds(0))),
            (NOW + 100, None),
            // Consistent with observations
            (NOW - 100, Some(DissolveState::DissolveDelaySeconds(0))),
            (NOW - 100, None),
            (u64::MAX, Some(DissolveState::WhenDissolvedTimestampSeconds(NOW))),
            (u64::MAX, Some(DissolveState::WhenDissolvedTimestampSeconds(NOW - 1))),
            (u64::MAX, Some(DissolveState::WhenDissolvedTimestampSeconds(0))),
        ];

        for (current_aging_since_timestamp_seconds, current_dissolve_state) in cases {
            test_increase_dissolve_delay_by_1_on_dissolved_neuron(
                current_aging_since_timestamp_seconds,
                current_dissolve_state,
            );
        }
    }

    #[test]
    fn increase_dissolve_delay_does_not_set_age_for_non_dissolving_neurons() {
        const NOW: u64 = 1000;
        fn test_increase_dissolve_delay_by_1_for_non_dissolving_neuron(
            current_aging_since_timestamp_seconds: u64,
            current_dissolve_delay_seconds: u64,
        ) {
            let mut non_dissolving_neuron = Neuron {
                aging_since_timestamp_seconds: current_aging_since_timestamp_seconds,
                dissolve_state: Some(DissolveState::DissolveDelaySeconds(
                    current_dissolve_delay_seconds,
                )),
                ..Default::default()
            };

            // Precondition - the neuron is non-dissolving
            assert_eq!(non_dissolving_neuron.state(NOW), NeuronState::NotDissolving);

            non_dissolving_neuron.increase_dissolve_delay(NOW, 1);

            assert_eq!(
                non_dissolving_neuron,
                Neuron {
                    // This field should be unaffected
                    aging_since_timestamp_seconds: current_aging_since_timestamp_seconds,
                    // This field's inner value should increment by 1
                    dissolve_state: Some(DissolveState::DissolveDelaySeconds(
                        current_dissolve_delay_seconds + 1
                    )),
                    ..Default::default()
                }
            );
        }

        // Test cases
        for current_aging_since_timestamp_seconds in [0, NOW - 1, NOW, NOW + 1, NOW + 2000] {
            for current_dissolve_delay_seconds in
                [1, 10, 100, NOW, NOW + 1000, (SECONDS_PER_DAY * 365 * 8)]
            {
                test_increase_dissolve_delay_by_1_for_non_dissolving_neuron(
                    current_aging_since_timestamp_seconds,
                    current_dissolve_delay_seconds,
                );
            }
        }
    }

    #[test]
    fn increase_dissolve_delay_set_age_to_u64_max_for_dissolving_neurons() {
        const NOW: u64 = 1000;
        fn test_increase_dissolve_delay_by_1_for_dissolving_neuron(
            current_aging_since_timestamp_seconds: u64,
            dissolved_at_timestamp_seconds: u64,
        ) {
            let mut neuron = Neuron {
                aging_since_timestamp_seconds: current_aging_since_timestamp_seconds,
                dissolve_state: Some(DissolveState::WhenDissolvedTimestampSeconds(
                    dissolved_at_timestamp_seconds,
                )),
                ..Default::default()
            };

            // Precondition - neuron is already dissolving
            assert_eq!(neuron.state(NOW), NeuronState::Dissolving);

            neuron.increase_dissolve_delay(NOW, 1);

            assert_eq!(
                neuron,
                Neuron {
                    aging_since_timestamp_seconds: u64::MAX,
                    dissolve_state: Some(DissolveState::WhenDissolvedTimestampSeconds(
                        dissolved_at_timestamp_seconds + 1
                    )),
                    ..Default::default()
                }
            );
        }

        for current_aging_since_timestamp_seconds in [0, NOW - 1, NOW, NOW + 1, NOW + 2000] {
            for dissolved_at_timestamp_seconds in
                [NOW + 1, NOW + 1000, NOW + (SECONDS_PER_DAY * 365 * 8)]
            {
                test_increase_dissolve_delay_by_1_for_dissolving_neuron(
                    current_aging_since_timestamp_seconds,
                    dissolved_at_timestamp_seconds,
                );
            }
        }
    }
}
