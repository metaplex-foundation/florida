FROM public.ecr.aws/lambda/nodejs:16

# build + local run commands
# =============================
# 1. docker build -t aws-deploy-signer .
# 2. docker run --rm -p 9000:8080 --name aws-deploy-signer aws-deploy-signer
# 3. curl -X POST \
#  -H "Content-Type: application/json" \
#  -d '{}' \
#  "http://localhost:9000/2015-03-31/functions/function/invocations"

# install required libudev (and more packages)
RUN yum update -y && \
 yum install -y systemd-devel

RUN sh -c "$(curl -sSfL https://release.solana.com/stable/install)"
ENV PATH="/root/.local/share/solana/install/active_release/bin:${PATH}"

ENV NODE_VERSION="v17.0.1"
ENV HOME="/root"
# ENV ANCHOR_CLI = "v0.25.0"

# Install rust.
RUN curl "https://sh.rustup.rs" -sfo rustup.sh && \
 sh rustup.sh -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Install anchor.
# todo: add tag 
# RUN cargo install --git https://github.com/coral-xyz/anchor --tag ${ANCHOR_CLI} anchor-cli --locked

# # Build a dummy program to bootstrap the BPF SDK (doing this speeds up builds).
# note: this currently fails due to some missing package error
# RUN cd /tmp && anchor init dummy && cd dummy && anchor build

COPY . ${LAMBDA_TASK_ROOT}

RUN npm install

# Set the CMD to your handler (could also be done as a parameter override outside of the Dockerfile)
CMD [ "/var/task/src/index.handler" ]
