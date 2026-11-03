#!/bin/sh
# 检查配置是否满足上线要求：timeout 必须是 60。
if grep -qx 'timeout = 60' config/settings.ini; then
  echo "CHECK OK: timeout = 60"
  exit 0
else
  echo "CHECK FAILED: timeout must be 60, got: $(grep '^timeout' config/settings.ini)"
  exit 1
fi
