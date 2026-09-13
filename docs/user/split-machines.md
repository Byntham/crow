# Run the service and worker separately

The default combined installation is simpler. Separate roles are useful when the public connection service belongs on one host and Codex should run on another. Both hosts still belong to the same Crow operator.

On the connection-service host:

```sh
crow setup --role service
crow pair
```

Service setup handles public HTTPS, your GitHub App, persistent startup, and storage. It does not require a Codex login. `crow pair` prints a service URL, worker ID, and secret token. Transfer these privately to the worker operator.

On the worker host:

```sh
crow setup --role worker
```

Enter the HTTPS service URL and pairing credentials. Complete Codex's device-code login on your desktop, confirm provider settings, and let setup enable persistent startup. This machine needs outbound HTTPS, but no Funnel, public listener, or GitHub App private key.

Back on the service host:

```sh
crow enroll owner/repo --worker WORKER_ID
crow status
```

Enrollment verifies your GitHub authority and associates the repository with that worker. Each repository has one assigned worker in this release. Multiple repositories may share a worker. Cross-machine load balancing and transfer of partially completed provider sessions are not implemented.

Run enrollment, author-policy changes, manual review commands, catch-up, and held-batch release on the service host. Run `crow login`, model discovery, and worker configuration on the worker host. Use the same `CROW_HOME` for all commands belonging to one installation.
