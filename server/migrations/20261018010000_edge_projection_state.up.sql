CREATE TABLE edge_path_state (
    map_revision TEXT NOT NULL CHECK (length(map_revision) = 64),
    from_node TEXT NOT NULL,
    to_node TEXT NOT NULL,
    is_active BOOLEAN NOT NULL,
    PRIMARY KEY (map_revision, from_node, to_node)
);
CREATE TABLE edge_forecast_state (
    singleton BOOLEAN PRIMARY KEY DEFAULT true CHECK (singleton),
    payload JSONB NOT NULL
);
