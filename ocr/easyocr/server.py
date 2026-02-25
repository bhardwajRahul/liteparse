import io
import logging
from typing import Any

import easyocr
import numpy as np
import uvicorn
from fastapi import FastAPI
from fastapi.datastructures import UploadFile
from fastapi.param_functions import File, Form
from PIL import Image
from pydantic import BaseModel


class OcrResponse(BaseModel):
    results: list[Any]


class StatusResponse(BaseModel):
    status: str


class EasyOCRServer:
    def __init__(self) -> None:
        self.reader: easyocr.Reader | None = None
        self.current_language: str | None = None

    def _create_ocr_server(self) -> FastAPI:
        app = FastAPI()

        @app.post("/ocr")
        async def ocr_endpoint(
            file: UploadFile = File(...), language: str = Form(default="en")
        ) -> OcrResponse:
            print(
                f"Language: {language}. Received file {file.filename or 'no name'} with size {file.size or 'unknown size'} and type {file.content_type or 'unknown type'}",
                flush=True,
            )
            # Get language from request
            language = language.lower()
            if self.reader is None or self.current_language != language:
                print(f"Initializing EasyOCR reader for language: {language}")
                self.reader = easyocr.Reader([language], gpu=False)
                self.current_language = language

            image_data = await file.read()
            image = Image.open(io.BytesIO(image_data))

            # Convert to numpy array
            image_array = np.array(image)

            # Run OCR
            results = self.reader.readtext(image_array)  # type: ignore

            # Format results according to LiteParse OCR API spec
            # Convert from EasyOCR format: [[[x1,y1], [x2,y2], [x3,y3], [x4,y4]], text, confidence]
            # To standard format: { text, bbox: [x1, y1, x2, y2], confidence }
            formatted = []
            for coords, text, confidence in results:
                # Convert polygon to axis-aligned bounding box
                # coords is [[x1,y1], [x2,y2], [x3,y3], [x4,y4]]
                if isinstance(coords, np.ndarray):
                    coords = coords.tolist()

                # int casting is necessary for pydantic serialization (np.Int32 are not serializable)
                xs = [int(point[0]) for point in coords]
                ys = [int(point[1]) for point in coords]
                bbox = [min(xs), min(ys), max(xs), max(ys)]

                formatted.append(
                    {"text": text, "bbox": bbox, "confidence": float(confidence)}
                )
            return OcrResponse(results=formatted)

        @app.get("/health")
        def health() -> StatusResponse:
            return StatusResponse(status="healthy")

        return app

    def serve(self) -> None:
        app = self._create_ocr_server()
        uvicorn.run(app, host="0.0.0.0", port=8828)


if __name__ == "__main__":
    logging.basicConfig(
        level=logging.DEBUG,
    )
    logging.info("Starting server on port 8828")
    server = EasyOCRServer()
    server.serve()
