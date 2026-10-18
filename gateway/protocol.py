"""Wire construction only; authority and event acceptance belong to the server."""

import json
from uuid import UUID


def encode_edge_event(event_id, payload, forwarded_at):
    UUID(event_id)
    if not isinstance(payload, dict):
        raise ValueError("Edge events require an object payload")
    return json.dumps(
        {
            "type": "EDGE_EVENT",
            "version": 1,
            "event_id": event_id,
            "payload": payload,
            "gateway_forwarded_at": forwarded_at,
        },
        allow_nan=False,
    )
