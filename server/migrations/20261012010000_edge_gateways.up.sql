CREATE TABLE edge_gateways (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    credential_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(credential_digest) = 32),
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    path_update BOOLEAN NOT NULL DEFAULT FALSE,
    trend_prediction BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE edge_camera_grants (
    gateway_id UUID NOT NULL REFERENCES edge_gateways(id) ON DELETE CASCADE,
    camera_id VARCHAR(50) NOT NULL,
    spot_update BOOLEAN NOT NULL DEFAULT FALSE,
    car_detected BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (gateway_id, camera_id)
);

CREATE TABLE edge_camera_spots (
    gateway_id UUID NOT NULL,
    camera_id VARCHAR(50) NOT NULL,
    spot_id TEXT NOT NULL REFERENCES spots(id) ON DELETE CASCADE,
    PRIMARY KEY (gateway_id, camera_id, spot_id),
    FOREIGN KEY (gateway_id, camera_id)
        REFERENCES edge_camera_grants(gateway_id, camera_id) ON DELETE CASCADE
);
