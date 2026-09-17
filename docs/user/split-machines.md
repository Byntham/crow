# Run the service and worker separately

The default combined installation is simpler. Separate roles are useful when the public connection service belongs on one host and Codex should run on another. Both hosts still belong to the same Crow operator.

These instructions apply to a new installation. An existing combined installation cannot switch to `service` while any repository or unfinished review is assigned to its local worker. Setup refuses the change before saving the new role, so the existing worker keeps running. `crow pair` creates another worker; it does not move existing repositories or saved sessions. Keep the existing installation in `both` mode until an explicit migration feature is available. You can pair remote workers with a combined installation and assign newly enrolled repositories to them. A service-hosting installation also cannot switch to `worker` while it holds repositories or unfinished reviews, because that would remove their connection service. Create the worker installation on the other machine instead.

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

One App can cover all your repositories across its GitHub installations. The service ignores repositories you have not enrolled. Pair each worker with this same service, then enroll its repositories with its worker ID. Workers authenticate separately and receive only their own jobs. Each job includes a short-lived GitHub token restricted to contents and metadata read access for that one repository. The service keeps the App private key and the write credentials used to publish reviews. Permission levels are fixed, not configurable per worker.

A worker's pairing token does not grant service administration or access to another worker's jobs. This does not isolate programs running under the same OS account: a worker sharing the service's account and filesystem can access its files. Use separate hosts or OS accounts when that distinction matters. Reassigning repositories does not revoke already issued GitHub tokens or delete existing source checkouts.

You can [connect an existing App](networking.md#connect-an-existing-github-app) during service setup. Do not connect the same App to separate active Crow services; its webhook destination is shared and separate service databases do not coordinate reviews.

Run enrollment, author-policy changes, manual review commands, catch-up, and held-batch release on the service host. Run `crow login`, model discovery, and worker configuration on the worker host. Use the same `CROW_HOME` for all commands belonging to one installation.
