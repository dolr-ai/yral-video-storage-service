YRAL VIDEO STORAGE SERVICE

Storj interface is a rest interface to upload objects to storj via the uplink cli.

## Configuration

Storj interface is configured via the following environment variables.

| Name                      | Description                                                    | Default (no default means `required`) |
|---------------------------|----------------------------------------------------------------|---------------------------------------|
| `STORJ_ACCESS_GRANT_SFW`  | Storj access grant that is used when accessing the sfw bucket  |                                       |
| `STORJ_ACCESS_GRANT_NSFW` | Storj access grant that is used when accessing the nsfw bucket |                                       |
| `SFW_BUCKET`              | The name of the sfw bucket                                     | yral-videos                           |
| `NSFW_BUCKET`             | The name of the nsfw bucket                                    | yral-nsfw-videos                      |
| `SERVICE_SECRET_TOKEN`    | Share secret between storj interface and the caller            |                                       |

For running locally, a storj account is required. 
- `cp .env.example .env`
- Create two buckets, for storing sfw and nsfw videos. Update `.env` file accordingly.
- Create access grants to the buckets. Update `.env` file accordingly.

## Running prebuilt image

Given an appropriate `.env` file, the prebuilt image can be run using docker.
```sh
docker run --env-file .env --rm -it -p 3000:3000 ghcr.io/dolr-ai/storj-interface:latest
```

This has only been tested with x86_64 linux so far.

## Running locally built image (linux)

Ensure that your system can build for target `x86_64-unknown-linux-musl`.

Given an appropriate `.env` file, a local image can be built and run like so.
```sh
cargo build --release --target=x86_64-unknown-linux-musl
docker build -t storj-interface . # or any tag you want
docker run --env-file .env --rm -it -p 3000:3000 storj-interface
```

## Local Testing

The rest api is tested via `hurl`, so make sure that is installed on the machine.
An example variable file is available at `test/example-test.vars`.

Following command can be used to test the rest api side of the storj interface's `duplication` request.

```sh
hurl test/duplicate.hurl --variables-file test/local-test.vars --test
```

For more thorough e2e testing,
create [linksharing links to your buckets](https://storj.dev/learn/concepts/linksharing-service)
and refer to [testing workflow](./.github/workflows/e2e-tests.yml).

## Deployment

- For pr previews and e2e testing, fly is used.
- For production deployment, theta is used.

## Why not use storj S3 interface

Storj provides an S3 compatible interface that in theory allows uploading videos
via any s3 client like the sdk or the aws cli. But, it has minor inconsistencies
in implementation and doesn't seem to be prioritized in case an incompatibility
occurs.

For example, https://github.com/storj/gateway-st/issues/89 breaks compatibility with `aws cli`.

## Why not use uplink crate

Uplink crate, as of writing this documentation, doesn't compile for `wasm32-unknown-unknown`.


## NOTE: Hetzner ie sfw upload is indirectly tested via move2nsfw test (the test that copies from hetzner to storj and then verifies on storj)

