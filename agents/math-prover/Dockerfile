# Minimal built-in example agent: asks the model for a proof of the stage's
# problem and writes it to answer.md. Standard library only.
FROM python:3.12-slim@sha256:dddfd7e07f9d15aeeca61529320492139d21cac7f0070c00609243e51e4e0016
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1
COPY agent.py /opt/agent/agent.py
RUN chmod -R a+rX /opt/agent
