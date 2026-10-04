# llm-judge scorer image: grades a written answer against a rubric with a
# judge model reached only through the platform's meter. Standard library only.
FROM python:3.12-slim@sha256:dddfd7e07f9d15aeeca61529320492139d21cac7f0070c00609243e51e4e0016
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 LANG=C.UTF-8 LC_ALL=C.UTF-8
COPY judge.py /opt/scorer/judge.py
RUN chmod -R a+rX /opt/scorer
