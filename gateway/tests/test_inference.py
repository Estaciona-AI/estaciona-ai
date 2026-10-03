from unittest.mock import patch

import pytest
import torch
from sqlalchemy import create_engine, text

from ml.inference import PredictiveEngine


class FixedForecast(torch.nn.Module):
    def forward(self, values):
        return torch.full((values.shape[0], 24), 2.0)


@pytest.mark.parametrize("spot_count", [0, 3, 35])
def test_forecast_capacity_comes_from_registered_spots(spot_count):
    # A real SQL table exercises capacity lookup without depending on a checkpoint.
    engine = create_engine("sqlite://")
    with engine.begin() as connection:
        connection.execute(text("CREATE TABLE spots (id TEXT PRIMARY KEY)"))
        for index in range(spot_count):
            connection.execute(
                text("INSERT INTO spots (id) VALUES (:id)"), {"id": str(index)}
            )

    predictor = PredictiveEngine.__new__(PredictiveEngine)
    predictor.engine = engine
    predictor.max_val = 44.0  # Training normalization is independent of site capacity.
    predictor.transformer = FixedForecast()
    with patch.object(predictor, "get_history", return_value=[0] * 192):
        payload = predictor.predict_trends()

    assert payload["max_capacity"] == spot_count
    assert len(payload["next_24h_occupancy"]) == 24
    assert all(
        0 <= item["occupancy"] <= spot_count for item in payload["next_24h_occupancy"]
    )
    engine.dispose()
