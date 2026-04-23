FROM postgres:16

ARG AGE_VERSION=PG16/v1.5.0-rc0
ARG PGMQ_VERSION=v1.4.4

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential git ca-certificates flex bison \
        postgresql-server-dev-16 \
        postgresql-16-pgvector \
    && rm -rf /var/lib/apt/lists/*

RUN git clone --depth 1 --branch ${AGE_VERSION} https://github.com/apache/age.git /tmp/age \
    && cd /tmp/age \
    && make PG_CONFIG=/usr/lib/postgresql/16/bin/pg_config \
    && make PG_CONFIG=/usr/lib/postgresql/16/bin/pg_config install \
    && rm -rf /tmp/age

RUN git clone --depth 1 --branch ${PGMQ_VERSION} https://github.com/tembo-io/pgmq.git /tmp/pgmq \
    && cd /tmp/pgmq/pgmq-extension \
    && make PG_CONFIG=/usr/lib/postgresql/16/bin/pg_config \
    && make PG_CONFIG=/usr/lib/postgresql/16/bin/pg_config install \
    && rm -rf /tmp/pgmq

COPY init-extensions.sql /docker-entrypoint-initdb.d/00-init-extensions.sql
