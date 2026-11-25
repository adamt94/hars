FROM python:3.12-slim

WORKDIR /app
COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

COPY roborock_plugin.py ./

ENV PYTHONUNBUFFERED=1
ENTRYPOINT ["python3", "-u", "/app/roborock_plugin.py"]
CMD ["--socket", "/config/plugins/run/roborock.sock"]
