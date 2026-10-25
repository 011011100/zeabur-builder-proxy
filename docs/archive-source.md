# Archive sources

A `Build` can build a folder that a client uploads, instead of a Git ref. The client packs the
folder, pushes it to the registry as an OCI artifact, and names the artifact's digest in the
`Build`:

```yaml
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: api, namespace: team-a}
spec:
  source:
    archive: {digest: "sha256:<manifest digest>"}
  builderRef: railpack
```

`source` holds exactly one of `git` and `archive`. An archive source takes `contextDir` and
`dockerfile` like a Git source, and no `credentialsRef`, `githubApp` or webhook trigger. To build
a new upload, change `digest`.

## Where the artifact goes

The repository is fixed by the `Build`:

```
<registry.host>/<namespace>/<build name>/source
```

The `Build` names only the digest. The Job downloads from its own namespace's repository, so a
`Build` cannot read the archive of another namespace or of another `Build`, even with the right
digest: the registry does not find the manifest there, and the run fails with `SourceFailed`.

## Who pushes it

The client that has the folder, such as Zeabur or `zeaburctl`, pushes it before it sets the
digest in the `Build`. The Job reads the archive with a copy of the run's registry credentials,
and trusts the registry CA of `registry.ca` or `registry.caFrom`.

- With `registry.tokenKey`: the client asks the `Build` for an upload token with the annotation
  `builder.zeabur.com/upload-token-request` and pushes with it
  ([README](../README.md#registry-tokens)). A new `Build` may leave `digest` unset until then.
  The run's token reads only its own `Build`'s archives.
- Without a token key, the push and the download are anonymous: only for a test registry.

Push the manifest with a tag, not only by digest. A registry that deletes untagged manifests,
such as the [Zeabur Catalog](https://github.com/zeabur-oss/catalog)'s Zot, may otherwise remove
it before the build starts.

## The artifact

An [OCI image manifest](https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md)
in the form `oras push` writes:

| Field | Value |
|---|---|
| `mediaType` | `application/vnd.oci.image.manifest.v1+json` |
| `artifactType` | `application/vnd.zeabur.source.v1+tar` |
| `config` | the empty descriptor: `application/vnd.oci.empty.v1+json`, `{}` |
| `layers` | exactly one, `application/vnd.oci.image.layer.v1.tar+gzip`: the folder as a gzip tar |

The `digest` in the `Build` is the manifest's digest. The Job checks the manifest and the layer
against their digests, and refuses a manifest of another type or with more than one layer.

With [ORAS](https://oras.land), from the folder, and `auth.json` from the `Build`'s upload
Secret ([README](../README.md#registry-tokens)):

```sh
tar -czf /tmp/source.tar.gz .
cd /tmp && oras push --registry-config auth.json \
  zot.zot.svc:5000/team-a/api/source:upload-$(date +%s) \
  --artifact-type application/vnd.zeabur.source.v1+tar \
  source.tar.gz:application/vnd.oci.image.layer.v1.tar+gzip
```

`oras push` prints the digest. `scripts/example-archive.sh` does the same with `curl`.

## Limits and unpack rules

- The layer is at most 256 MiB compressed, and the tar stream at most 1 GiB decompressed. The
  unpack stops at the limit.
- An entry with an absolute path or `..` stops the unpack.
- A symbolic link must stay inside the folder: a relative target that does not climb above the
  folder's root. A hard link must name a regular file earlier in the archive.
- Nothing is written through a symbolic link.
- Device files and FIFOs are skipped.
- Files get mode `0755` when the archive gives an execute bit, else `0644`. Owners and times are
  not kept.

## Status

The `BuildRun`'s `revision` and the `Build`'s `status.remoteRevision` and
`status.latestRevision` hold the manifest digest. `status.resolvedRef` stays empty. Tags use
`archive` as the branch and the first seven hexadecimal digits of the digest:
`archive-1761058800-3e1a9c2`. No commit status is sent.

## Old archives

Each upload stays in the registry until the registry removes it, or until its `Build` is
deleted ([cleanup](../README.md#registry-tokens)). Zot in the Zeabur Catalog
keeps the 20 most recently pushed tags of each repository and deletes untagged manifests after
two hours, so `<namespace>/<build>/source` keeps the last 20 uploads. For another registry, set
a retention policy for `*/*/source`.
