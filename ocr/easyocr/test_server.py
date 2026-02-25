import io
from typing import Any

import pytest
from fastapi.testclient import TestClient
from PIL import Image

from server import EasyOCRServer


@pytest.fixture(scope="module")
def server() -> EasyOCRServer:
    return EasyOCRServer()


class MockEasyOCRReader:
    def __init__(self, *args, **kwargs) -> None:
        self.results = [
            ([[10, 20], [200, 40]], "Hello World", 0.98),
            ([[10, 50], [250, 70]], "Total: $42.00", 0.95),
            ([[10, 80], [180, 100]], "Thank you!", 0.87),
        ]
        self.transformed_results = [
            {"text": "Hello World", "bbox": [10, 20, 200, 40], "confidence": 0.98},
            {"text": "Total: $42.00", "bbox": [10, 50, 250, 70], "confidence": 0.95},
            {"text": "Thank you!", "bbox": [10, 80, 180, 100], "confidence": 0.87},
        ]

    def readtext(self, *args, **kwargs) -> list[Any]:
        return self.results


def test_server_init(server: EasyOCRServer) -> None:
    assert server.current_language is None
    assert server.reader is None


def test_server_health_endpoint(server: EasyOCRServer) -> None:
    app = server._create_ocr_server()
    client = TestClient(app)
    response = client.get("/health")
    assert response.status_code == 200
    assert response.json() == {"status": "healthy"}


def test_server_ocr_endpoint(server: EasyOCRServer) -> None:
    image = Image.new("RGB", (1, 1), color=(255, 255, 255))

    # Save to bytes (to simulate a file upload)
    buffer = io.BytesIO()
    image.save(buffer, format="PNG")
    buffer.seek(0)
    app = server._create_ocr_server()
    mock_ocr = MockEasyOCRReader()
    server.reader = mock_ocr  # type: ignore
    server.current_language = "en"
    client = TestClient(app)

    response = client.post(
        "/ocr",
        files={"file": ("test.png", buffer, "image/png")},
        data={"language": "en"},
    )
    assert response.status_code == 200
    assert response.json().get("results", []) == mock_ocr.transformed_results
