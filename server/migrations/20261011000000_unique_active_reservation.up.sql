-- Reject legacy duplicates rather than silently cancelling a reservation.
CREATE UNIQUE INDEX reservations_one_active_per_spot
ON reservations (spot_id) WHERE status = 'active';
