# Uploaded scorer example (docs/plugins.md §14): standard library only.
FROM python:3.12-slim@sha256:dddfd7e07f9d15aeeca61529320492139d21cac7f0070c00609243e51e4e0016
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1
COPY score.py /opt/scorer/score.py
ENTRYPOINT ["python3", "/opt/scorer/score.py"]
