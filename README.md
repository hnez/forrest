Forrest - A GitHub Action Runner Runner
=======================================

                                        ┏━━━━━━━━━━━━━━━┓
                                        ┃      Run      ┃
                                        ┃    Forrest    ┃
                                        ┃      Run      ┃
                                        ┗━━━┯━━━━━━━┯━━━┛

> CI jobs are like a box of chocolates. You never know what you're gonna get.
>
>       - Some dude on GitHub

And that's why Forrest runs GitHub actions in single-use virtual machines.

How it works
------------

Right now Forrest is designed to concurrently run multiple virtual machines
as GitHub action runners on a single host computer.
Running virtual machines across a cluster of hosts is currently out of scope.

The main ingredients to set up Forrest are:

- A host

  The host needs to be reachable from the internet so that GitHub can deliver
  webhooks to it, to notify it about e.g. new jobs to run.

  The host also needs to use a reflink-capable filesystem, e.g. Btrfs or XFS,
  so that disk images can be copied around cheaply.

- A GitHub application

  Forrest is designed to authenticate with the GitHub API as an App,
  because they provide a non-expiring authentication method.

  You can create a new GitHub App in the
  [developer settings](https://github.com/settings/apps).

  You need to:

  - Generate a client secret to use for authentication later.
  - Provide a webhook URL _with a secret_ that terminates at a reverse proxy
    on the host running Forrest.
    See [Setting up nginx](#setting-up-nginx) for an example on how to
    configure nginx as reverse proxy for Forrest.
  - Enable Read and Write "Actions", "Administration" (to add jit runners)
    and "Contents" repository permissions for the app.
  - Enable "Workflow job" events for the app.
  - Install the app for your user/app.

- A base image and accompanying setup instructions

  The virtual machines need an initial image to boot from.
  The image needs to have [cloud-init](https://cloudinit.readthedocs.io/en/latest/)
  pre-installed and needs to be a raw disk image (e.g. not qcow).

  In addition to the image itself you will need a set of scripts and config
  files to set up the GitHub action runner software on the machine.
  See [`contrib/seeds/debian`](contrib/seeds/debian) for a Debian example config.

  A correctly set up Forrest environment directory will look something like this:

  ```bash
  $ tree env
  env
  └── seeds
      └── debian-12
          ├── cloud-init
          │   ├── meta-data
          │   └── user-data
          ├── job-config
          │   └── job.sh
          └── debian-12-genericcloud-amd64.raw
  ```


- A `config.yaml` file

  The config file contains some information about the host (the amount RAM
  available for virtual machines and the directory to place things under),
  how to access GitHub, information about our "machines" and about the
  repositories Forrest should serve.

  ```yaml
  host:
    ram: 120G
    base_dir: env

  github:
    app_id: <APP_ID>
    jwt_key_file: key.pem
    webhook_secret: <WEBHOOK_SECRET>

  machine_templates:
    small: &machine-small
      seed: debian-12
      ram: 7500M
      cpus: 4
      disk: 16G
    large: &machine-large
      seed: debian-12
      ram: 30G
      cpus: 10
      disk: 64G

  repositories:
    hnez:
      forrest:
        persistence_token: <PERSISTENCE_TOKEN>
        machines:
          check: *machine-small
          build: *machine-large
  ```

With the setup done we can write our first GitHub workflow using Forrest:

```yaml
name: demo

on: [pull_request, push]

jobs:
  demo:
    name: Demo Job
    runs-on: [self-hosted, forrest, check]
    steps:
      - name: Set up runner machine
        run: |
          sudo localectl set-locale en_US.UTF-8
          sudo apt-get update
          sudo apt-get --assume-yes dist-upgrade
          sudo apt-get --assume-yes install git
      - name: Check out the repository
        uses: actions/checkout@v4
      - name: Demo
        run: echo "Hey there from a machine managed by Forrest!"
      - name: Persist the disk image
        env:
          PERSISTENCE_TOKEN: ${{ secrets.PERSISTENCE_TOKEN }}
        if: ${{ env.PERSISTENCE_TOKEN != ''  }}
        run: |
          sudo fstrim /
          echo "$PERSISTENCE_TOKEN" > ~/config/persist
```

You may notice that the job does a lot of setup and post-processing.
This is because we use very bare disk images to begin with.
Forrest only installs the GitHub runner software itself onto the machines
and leaves the rest to the scheduled job.

The final trick is the `PERSISTENCE_TOKEN`.
It is configured per-repository in your `config.yaml` and is also stored as
an action secret on GitHub, that is only available to jobs running on your
protected branches (e.g. `main`).

If a job runs for such a protected branch and decides to persist its current
disk image, then this disk image will become the new starting disk image for
subsequent runs of the same machine.
In this case the `check` machine for the repository.
This way installed packages and other downloads are already present on the
machine, speeding up subsequent runs a lot.

---

### Setting up nginx

In your nginx config under the `server` section you should add a proxy directive
to forward requests to Forrest:

```
server {
    listen 443 ssl http2 default_server;
    listen [::]:443 ssl http2 default_server;

    ...

    location /webhook {
        proxy_pass http://unix:[ABOLUTE PATH TO YOUR FORREST ENV]/webhook.sock:/webhook;
        proxy_http_version 1.1;
    }
}
```

Replace `[ABOLUTE PATH TO YOUR FORREST ENV]` with the appropriate path.

### Debugging a running job

All (running) jobs have a `shell.sock` unix domain socket in their run directory
(e.g. in `[FORREST ENV PATH]/runs/[USER]/[REPO]/[MACHINE TYPE]/[TIMESTAMP]`)
that can be used to log into the machine using e.g. `socat`:

```bash
$ socat -,rawer,escape=0x1d UNIX-CONNECT:.../shell.sock
```

> [!NOTE]
> You need to press enter to get an initial prompt

---

                                        ┏━━━━━━━━━━━━━━┓
                                        ┃     Stop     ┃
                                        ┃    Forrest   ┃
                                        ┃     Stop     ┃
                                        ┗━━━┯━━━━━━┯━━━┛

