FROM ghcr.io/astral-sh/uv:python3.12-trixie

# Install system dependencies
RUN apt-get update && apt-get install -y \
    libgomp1 \
    libglib2.0-0 \
    libsm6 \
    libxext6 \
    libxrender-dev \
    libgl1 \
    && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy pyproject.toml and server code
COPY ./*py* .

# install necessary dependencies
RUN uv sync

# Expose port
EXPOSE 8828

# Run server
CMD ["uv", "run", "python3", "server.py"]
