Arch Linux Machine Images
=========================

The Arch Linux project provides virtual machine disk images suitable for use
with Forrest.
They can be downloaded from any Arch Linux mirror under the `/images/`
subdirectory:

```bash
$ curl --location --compressed -o Arch-Linux-x86_64-cloudimg.qcow2 \
  https://mirrors.edge.kernel.org/archlinux/images/latest/Arch-Linux-x86_64-cloudimg.qcow2
```

The Arch Linux images are distributed in the qcow2 format,
but Forrest assumes raw image files.
The `qemu-img`-tool can be used to convert between the two:

```bash
$ qemu-img convert -O raw \
  Arch-Linux-x86_64-cloudimg.qcow2 \
  Arch-Linux-x86_64-cloudimg.img

$ rm Arch-Linux-x86_64-cloudimg.qcow2
```

Systemd and cloud-init make the Debian and Arch Linux images similar enough
that the `cloud-init` and `job-config` directories for the Debian images can
be reused.
