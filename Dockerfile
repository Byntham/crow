FROM node:22-bookworm-slim

ARG CROW_CLI_PACKAGE=@openai/codex
RUN apt-get update \
  && apt-get install -y --no-install-recommends git ca-certificates openssh-client bubblewrap \
  && npm install --global "${CROW_CLI_PACKAGE}" \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev
COPY server ./server

RUN mkdir -p /app/.crow-data /home/node/.codex /home/node/.claude \
  && chown -R node:node /app /home/node
USER node
ENV NODE_ENV=production HOME=/home/node
EXPOSE 8787
CMD ["node", "server/crow.mjs"]
