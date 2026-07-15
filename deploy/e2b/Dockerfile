# syntax=docker/dockerfile:1.7

ARG GO_IMAGE=golang:1.26.3-bookworm
ARG RUNTIME_IMAGE=node:20-bookworm-slim

FROM ${GO_IMAGE} AS envd-builder

ARG TARGETARCH
ARG E2B_INFRA_COMMIT=fda7bef1095afb909197e272c0a8a123797f0bfb

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
RUN git init /src/infra \
    && git -C /src/infra remote add origin https://github.com/e2b-dev/infra.git \
    && git -C /src/infra fetch --depth=1 origin "${E2B_INFRA_COMMIT}" \
    && git -C /src/infra checkout --detach FETCH_HEAD \
    && test "$(git -C /src/infra rev-parse HEAD)" = "${E2B_INFRA_COMMIT}" \
    && grep -F 'const Version = "0.6.9"' /src/infra/packages/envd/pkg/version.go
WORKDIR /src/infra/packages/envd
RUN CGO_ENABLED=0 GOOS=linux GOARCH="${TARGETARCH}" \
    go build -trimpath -buildvcs=false -a \
    -ldflags "-X=main.commitSHA=${E2B_INFRA_COMMIT} -s -w -buildid=" \
    -o /out/envd .

FROM debian:bookworm-slim AS code-interpreter-source

ARG CODE_INTERPRETER_COMMIT=5aeca43fe3fae2df260b1fb17c71fed5b5dac852

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
RUN git init /src/code-interpreter \
    && git -C /src/code-interpreter remote add origin https://github.com/e2b-dev/code-interpreter.git \
    && git -C /src/code-interpreter fetch --depth=1 origin "${CODE_INTERPRETER_COMMIT}" \
    && git -C /src/code-interpreter checkout --detach FETCH_HEAD \
    && test "$(git -C /src/code-interpreter rev-parse HEAD)" = "${CODE_INTERPRETER_COMMIT}"

FROM ${RUNTIME_IMAGE}

ARG E2B_INFRA_COMMIT=fda7bef1095afb909197e272c0a8a123797f0bfb
ARG CODE_INTERPRETER_COMMIT=5aeca43fe3fae2df260b1fb17c71fed5b5dac852
ARG IJAVASCRIPT_COMMIT=79cb7d56dcfe0df9ff55eff0ca4dc920a6dcc361

LABEL org.opencontainers.image.title="A3S Box E2B runtime template" \
      org.opencontainers.image.description="Pinned envd and Code Interpreter services for A3S Box Sandboxes" \
      org.opencontainers.image.source="https://github.com/A3S-Lab/Box" \
      org.opencontainers.image.licenses="Apache-2.0" \
      io.a3s.e2b.infra.commit="${E2B_INFRA_COMMIT}" \
      io.a3s.e2b.code-interpreter.commit="${CODE_INTERPRETER_COMMIT}" \
      io.a3s.e2b.envd.version="0.6.9"

SHELL ["/bin/bash", "-o", "pipefail", "-c"]
RUN printf 'Acquire::Retries "5";\nAcquire::https::Timeout "30";\n' \
      >/etc/apt/apt.conf.d/80-a3s-retries \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
      bash build-essential ca-certificates curl git python3 python3-pip python3-venv \
      sudo tini util-linux \
    && rm -rf /var/lib/apt/lists/*

ENV PATH="/opt/a3s/e2b/jupyter/bin:/opt/a3s/e2b/code-interpreter/.venv/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
    PYTHONUNBUFFERED=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1

RUN python3 -m venv /opt/a3s/e2b/jupyter \
    && /opt/a3s/e2b/jupyter/bin/pip install \
      e2b-charts==1.0.0 \
      ipykernel==6.31.0 \
      ipython==9.15.0 \
      jupyter-server==2.20.0 \
      matplotlib==3.10.9 \
      numpy==2.3.5 \
      orjson==3.11.9 \
      pandas==2.2.3 \
      pillow==12.2.0 \
    && /opt/a3s/e2b/jupyter/bin/ipython kernel install --name python3 --prefix /usr/local \
    && npm install --global --unsafe-perm \
      "git+https://github.com/e2b-dev/ijavascript.git#${IJAVASCRIPT_COMMIT}" \
    && ijsinstall --install=global

COPY --from=envd-builder /out/envd /usr/local/bin/envd
COPY --from=code-interpreter-source /src/code-interpreter/template/server/ \
    /opt/a3s/e2b/code-interpreter/
COPY --from=code-interpreter-source /src/code-interpreter/template/jupyter_server_config.py \
    /opt/a3s/e2b/config/jupyter_server_config.py
COPY --from=code-interpreter-source /src/code-interpreter/template/ipython_kernel_config.py \
    /opt/a3s/e2b/config/ipython_kernel_config.py
COPY --from=code-interpreter-source /src/code-interpreter/template/startup_scripts/ \
    /opt/a3s/e2b/config/startup/
COPY --from=code-interpreter-source /src/code-interpreter/LICENSE \
    /usr/share/licenses/e2b-code-interpreter/LICENSE
COPY --from=envd-builder /src/infra/LICENSE /usr/share/licenses/e2b-infra/LICENSE

RUN python3 -m venv /opt/a3s/e2b/code-interpreter/.venv \
    && /opt/a3s/e2b/code-interpreter/.venv/bin/pip install \
      -r /opt/a3s/e2b/code-interpreter/requirements.txt \
    && useradd --create-home --shell /bin/bash user \
    && install -d -o user -g user \
      /home/user/.ipython/profile_default/startup \
      /home/user/.jupyter \
    && cp /opt/a3s/e2b/config/jupyter_server_config.py \
      /home/user/.jupyter/jupyter_server_config.py \
    && cp /opt/a3s/e2b/config/ipython_kernel_config.py \
      /home/user/.ipython/profile_default/ipython_kernel_config.py \
    && cp /opt/a3s/e2b/config/startup/* \
      /home/user/.ipython/profile_default/startup/ \
    && chown -R user:user /home/user \
    && printf 'user ALL=(ALL) NOPASSWD: ALL\n' >/etc/sudoers.d/a3s-box-user \
    && chmod 0440 /etc/sudoers.d/a3s-box-user \
    && ln -s /usr/bin/python3 /usr/local/bin/python \
    && envd -version | grep -Fx '0.6.9'

COPY deploy/e2b/init-envd.py /usr/local/lib/a3s-box-e2b/init-envd.py
COPY deploy/e2b/entrypoint.sh /usr/local/bin/a3s-box-e2b-entrypoint

RUN chmod 0755 /usr/local/bin/a3s-box-e2b-entrypoint \
    && chmod 0644 /usr/local/lib/a3s-box-e2b/init-envd.py

WORKDIR /home/user
EXPOSE 49983 49999
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/a3s-box-e2b-entrypoint"]
