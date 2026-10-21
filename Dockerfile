FROM python:3.12-slim

WORKDIR /app
RUN pip install --no-cache-dir "duckdb>=1.1"
COPY finance.py rules.toml ./

ENTRYPOINT ["python", "finance.py"]
