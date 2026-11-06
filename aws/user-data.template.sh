#!/bin/bash
# Vecna emotion worker — first-boot setup.
# Placeholders replaced by launch script: __SERVER_B64__ __API_KEY__
set -euxo pipefail

export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y python3.12 python3.12-venv ffmpeg git

python3.12 -m venv /opt/venv
/opt/venv/bin/pip install --upgrade pip
# torch CPU wheels come from the pytorch index; everything else from PyPI
/opt/venv/bin/pip install --quiet torch --index-url https://download.pytorch.org/whl/cpu
/opt/venv/bin/pip install --quiet \
  transformers librosa soundfile numpy compressed-tensors \
  fastapi "uvicorn[standard]" python-multipart huggingface_hub

# 1) model weights (int8 quantized build, ~370MB), stored on this instance's disk
/opt/venv/bin/hf download Aniemore/wavlm-emotion-v1-crosslingual --local-dir /opt/emotion-model \
  --include "int8/*" --include "config.json" --include "preprocessor_config.json" \
  --include "tokenizer_config.json" --include "vocab.json" --include "special_tokens_map.json"

# 2) inference server
echo '__SERVER_B64__' | base64 -d > /opt/emotion_server.py

# 3) systemd service
cat > /etc/systemd/system/emotion.service <<EOF
[Unit]
Description=Vecna emotion inference server
After=network.target

[Service]
Type=simple
Environment=EMOTION_API_KEY=__API_KEY__
Environment=EMOTION_MODEL_DIR=/opt/emotion-model
Environment=EMOTION_MODEL_SUBFOLDER=int8
WorkingDirectory=/opt
ExecStart=/opt/venv/bin/uvicorn emotion_server:app --host 0.0.0.0 --port 8080
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now emotion.service
