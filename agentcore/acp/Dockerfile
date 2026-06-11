FROM python:3.12-slim

WORKDIR /app
COPY pyproject.toml agentcore_acp.py ./
RUN pip install --no-cache-dir .

ENTRYPOINT ["agentcore-acp"]
