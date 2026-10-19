-- Physical observations are independent of personal reservation history.
-- Hourly last-observation samples, not a complete transition audit or time average.
CREATE TABLE physical_occupancy_hourly (
    spot_id TEXT NOT NULL REFERENCES spots(id),
    hour_start TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('free', 'occupied')),
    observed_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (hour_start, spot_id)
);
