# Warmup for rsemu-server with configs/vmagent.goemu: runs node, the
# test runner, npm and python once in the base VM, so that their code is in the
# JIT code cache of all VMs.
cd /tmp && printf 'import t from "node:test";\nt("x", () => {});\n' > w.test.mjs && node --test w.test.mjs >/dev/null && node -e 'fetch; require("crypto").createHash("sha256")' && npm --version >/dev/null && python3 -c "import json, hashlib, sqlite3" && rm -rf w.test.mjs /root/.npm/_logs && cd /root
