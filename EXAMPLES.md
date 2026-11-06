# Examples

Every command here runs on your laptop, in this repo's folder. The `runner` on your laptop is a small wrapper: each command but `proxy` runs on the server over SSH (`ssh runner@kvm-host runner ...`), through Tailscale.

**Before you start:** the server is set up (README.md, "Set up a host" and "On AWS"), Tailscale is connected on your laptop, and `AWS_PROFILE=runner` is set (`aws login --profile runner` if it's been more than 12 hours).

## Words

- **Server (host):** the EC2 machine that runs everything, `kvm-host`. You can add more (section 9).
- **VM:** a small virtual machine on the server, one per agent, with its own Linux. You give each a name, like `dev`.
- **base:** the VM every new VM is copied from: Ubuntu with Claude Code.
- **Checkpoint:** a saved copy of a VM's disk that you can go back to.
- **Sleep:** a VM's memory saved to disk and the VM paused, so it costs nothing until it wakes.

## 1. Turn the server on and off

```sh
runner list            # turns the server on first if it's off, a few minutes, then lists its VMs
client/aws.sh start    # on, every server: a few minutes; each turns itself off after an hour with nothing running
client/aws.sh stop     # off now, every server: about $0.10 an hour each while on, and only its disks while off
client/aws.sh ssh      # log in to the server itself (rarely needed)
```

Any `runner` command turns its server on if it's off, saying so while it boots, then runs. With more than one server, only the one the command goes to (section 9).

## 2. Make, list and remove VMs

```sh
runner create demo     # a new VM named demo, copied from base: instant
runner list            # every VM, and the disk space each uses
runner start demo      # boot it (most commands below boot it for you)
runner status demo     # running or stopped
runner stop demo       # shut it down cleanly, which saves its disk
runner delete demo     # remove it, checkpoints and all (stop it first)
```

A VM keeps the base it was made from. Once base is rebuilt, say with a newer Claude Code, `rebase` moves an older VM onto it, keeping its files:

```sh
runner stop demo
runner rebase demo     # base's system, demo's /home, /root and services
runner undo demo       # changed your mind: demo's old disk is back
```

It prints what it kept and warns about the rest:

```
demo is on base's generation 4 now: generation 13 is current.
Kept /home, /root, /etc/machine-id and 2 services (api, web).
Anything installed outside those, like apt packages, is gone, and so are users added with useradd, crontabs and /var: runner undo demo brings it back.
```

So reinstall your apt packages after it. Claude Code comes from base, at base's version.

A VM gets 2 CPUs, 2 GB of memory and base's 20 GB disk unless you say otherwise. To give one more, stop it first:

```sh
runner stop demo
runner resize demo --disk 40G --mem 8G --cpus 4    # any of the three; it's that big from its next start
runner status demo                                 # shows its disk, cpus and memory
runner undo demo                                   # the disk as it was before the resize
runner create big --disk 40G --mem 8G              # or make a new VM that big from the start
```

A disk only grows, and your files stay as they were.

## 3. Work in a VM

```sh
runner claude demo     # Claude Code running inside demo
runner shell demo      # a terminal inside demo
runner exec demo -- ls /home/agent/work    # run one command inside demo
runner service add demo web -- python3 -m http.server 8080   # a server that's up whenever demo is
runner service list demo    # its services, and whether each is running
runner service logs demo web    # what web printed
runner service rm demo web  # stop web for good
runner proxy demo 8080      # open localhost:8080 on your laptop: demo's web server, until Ctrl-C
```

A service starts again on its own: when it exits, and every time the VM starts.

In `runner claude`, press **Ctrl-b**, then **d**, to leave Claude running in the VM. Run `runner claude demo` again to come back.

Your files live in `/home/agent/work` inside the VM.

## 4. Run an agent without watching

```sh
runner run demo "Add a README to /home/agent/work"   # prints a run id, like demo.1
runner output demo.1 --follow                         # watch what it does, live
runner result demo.1                                  # the summary once it's done
```

If `demo` was stopped, the run boots it and keeps the changes only if the agent finishes without an error. If it was running, the run leaves it running, changes and all, even when the agent fails, so your own sessions in it carry on. The run saves a checkpoint first, and `runner result demo.1` gives its id: `runner restore demo <id>` undoes the run.

To carry on where a run left off, give `--resume` the session `runner runs demo` shows for it, a long id like `0f8b2c4e-…`. It works for a run whose changes were kept, since the session is saved on demo's disk:

```sh
runner runs demo                                          # each run, with its session once the agent has started
runner run demo --resume <session> "Now add a LICENSE too"
```

To step into a run that's still going and steer it yourself, take it over:

```sh
runner claude --run demo.1   # stops the run's agent, then opens its conversation in Claude Code in demo
```

The run's changes stay, and demo stays on: from there it's the same as `runner claude demo`, and **Ctrl-b**, then **d**, leaves it running. It works once `runner runs demo` shows the run's session, and not while `runner claude demo` is already open (Ctrl-b, then d, doesn't close it: `runner kill demo claude` does).

To have several agents try the same thing at once and keep the best, give `--n`:

```sh
runner run demo --n 3 "Make the flaky test pass"   # prints demo-1.1, demo-2.1 and demo-3.1
runner output demo-2.1 --follow                     # watch one try
runner tries demo --wait                            # all three side by side, once they're done
runner result demo-1.1                              # what that try changed, path by path
```

`runner tries demo` gives a row per try, so you can pick one at a glance:

```
RUN       STATE                  EXIT  TOOK   CHANGED  AGENT SAYS (UNTRUSTED)
demo-1.1  exited 0, committed    0     2m14s  2        "Fixed the flaky test."
demo-2.1  exited 1, rolled back  1     1m03s  1        "I couldn't make it pass: the fixture is missing."
demo-3.1  exited 0, committed    0     3m40s  9        "All tests pass."
```

CHANGED is how many paths the runner itself found changed on each try's disk. AGENT SAYS is only what the agent claims, so check it against `runner result` before you trust it. `--json` gives all of it, every try's changed paths included.

Each try is a new copy of `demo`, named `demo-1` to `demo-3`, so `demo` itself stays as it was. Three tries cost three times the tokens, and take three times `demo`'s memory: the runner refuses a batch the server has no room for and says how many fit.

Once you've picked, keep it:

```sh
runner keep demo-1            # demo gets demo-1's work; all three tries are deleted
runner run demo --resume <session> "Now add a test for it"   # keep prints the session
runner undo demo              # demo as it was before the keep
runner keep demo --none       # or keep none: delete the tries, demo stays as it was
```

`keep` gives `demo` the kept try's disk, and `demo` keeps its name, address, URL and credentials. A try still running is cancelled first. A try whose run rolled back has nothing of its own to keep, so it's refused. A running `demo` is restarted onto the new disk. A new batch can start once the tries are gone.

## 5. Checkpoints: an undo button for the whole VM

```sh
runner checkpoint demo -m "good state"   # save: prints its id, say 2
runner checkpoints demo                  # list them; * marks the current one
runner restore demo 2                    # go back to 2 (the VM restarts)
runner stop demo && runner undo demo     # undo the restore (undo needs the VM stopped)
```

A full round trip:

```sh
runner exec demo -- sh -c 'echo hi > /home/agent/work/notes.txt'
runner checkpoint demo -m "has notes"                  # say it prints id 2
runner exec demo -- rm /home/agent/work/notes.txt      # "break" it
runner restore demo 2
runner exec demo -- ls /home/agent/work                # notes.txt is back
```

Your checkpoints stay until you delete the VM. The ones the runner makes for itself, before a run on a running VM and when the agent inside asks, are cleaned up: it keeps the newest 20 of those.

## 6. Sleep: pause a VM, programs and all

```sh
runner sleep demo      # demo's memory goes to disk: it uses no CPU or RAM
runner status demo     # sleeping
runner wake demo       # back in about a second, every program where it was
```

`runner start`, `exec`, `shell`, `claude`, `sessions`, `attach`, `kill`, `run` and `checkpoint` wake it too. `runner stop` wakes it first, so it still shuts down cleanly and saves its disk.

You don't have to: a VM nobody has used for 10 minutes goes to sleep by itself, and the next command wakes it. A terminal attached to a tmux session keeps it awake; a detached one doesn't.

```sh
runner idle demo 30      # after 30 minutes instead; or 90s, 2h, never
```

A web app in a VM gets a link on your tailnet, and opening it wakes the VM if it's asleep:

```sh
runner service add demo web -- python3 -m http.server 8080
runner url demo          # http://100.x.y.z:10001: open it in the laptop's browser
```

## 7. Let Claude do it

**Claude inside the VM.** In `runner claude demo`, ask in plain words:

> Checkpoint this VM with the note "before refactor", then make the change.

> Restore the "before refactor" checkpoint.

It knows how from `/.runner/llm.txt` in the VM. A restore reboots the VM, which ends that Claude session, so run `runner claude demo` again after.

**Claude on your laptop.** Connect it once:

```sh
claude mcp add --scope user runner -- ~/.local/bin/runner mcp
```

Then in `claude` on your laptop:

> Use the runner MCP: checkpoint demo as "safe point", run an agent in demo to clean up /home/agent/work, show me the result, and restore "safe point" if anything looks wrong.

## 8. Back up a VM, and bring it back

```sh
runner export demo > demo.tar                             # its disk, your checkpoints and its settings, in one file
set -o pipefail; runner export demo | aws s3 cp - s3://<bucket>/demo.tar   # or straight to S3
tar -tf demo.tar                                          # a manifest, then each disk
aws s3 cp s3://<bucket>/demo.tar - | runner import demo2 -   # back as a new VM, here or on another host
runner creds demo2 claude github                          # if demo had github: an import gets claude only
runner start demo2                                        # its files and services, at its own address
```

demo carries on while it's exported: the disk is a copy made with demo paused for a moment, after a `sync`, as a checkpoint is, and no checkpoint is left behind. A sleeping demo is woken for it; one with a run going is refused until the run is over. The file holds only what's on the disks, so a 20 GiB VM with 4.6 GB on it makes a file of about 4.6 GB, and each of your checkpoints adds about as much again. `--no-checkpoints` leaves them out, and `| zstd` makes it smaller. `set -o pipefail` makes a failed export fail the whole line, so S3 isn't left holding half a backup you think is whole.

`import` brings your checkpoints back under the same ids, so `runner restore demo2 4` is the checkpoint 4 demo had. A cut-off download is refused and leaves no `demo2`, so just run it again.

## 9. More than one server

```sh
RUNNER_AWS_NAME=kvm-host-2 client/aws.sh up   # make a second server, set up like the first
runner hosts add runner@kvm-host-2   # then add it: the servers learn each other's keys
runner hosts                         # each server: its VMs, how many are up, its free memory
runner create demo3                  # goes to the server with the most free memory
runner list                          # every server's VMs, each line led by its server
runner shell demo3                   # finds demo3's server for you
runner create demo4 --host kvm-host  # or pick the server yourself
runner move demo3 kvm-host-2         # move it to another server, checkpoints and all
```

A VM name is used once across all your servers, so `demo3` always means the same VM, and every command naming it goes to its server. `base` is on each server. A server that's off is named, and the rest still listed: `runner list` turns none on, but a command naming a VM on it, or `--host kvm-host-2`, turns that one on. `client/aws.sh stop` and `start` turn every server off and on together (section 1). Other `client/aws.sh` commands take `RUNNER_AWS_NAME=kvm-host-2` for the second server; without it, they mean `kvm-host`. `RUNNER_AWS_NAME=kvm-host-2 client/aws.sh stop` turns off just that one.

Each server runs the build it was last given: `aws.sh up` gives kvm-host-2 this checkout's, and a plain `client/check.sh` updates only kvm-host. Keep both on the same build (README.md, After merging a change), so a move or Claude reaching across servers finds the same `runner` on both.

`move` keeps the name, your checkpoints, its settings, services and credentials. A running demo3 is shut down first, which ends what runs in it, and started again on the new server; its address and URL change, so it prints the new URL. The copy goes straight from server to server, and demo3 leaves the old one only once the new one has it all, so a move cut off halfway leaves it where it was.

## 10. Keep a VM to a few sites

```sh
runner net demo allow github.com registry.npmjs.org   # demo reaches only these, and Claude's API
runner net demo list                                   # what it can reach
runner net demo deny registry.npmjs.org                # take one off
runner net demo open                                   # the whole internet again
```

It works at once, on a running VM too, with no restart. A name covers the names under it: github.com lets in api.github.com. Any other site doesn't resolve inside demo, and Claude there knows to ask you rather than work around it. Claude on your laptop can read the list (`vm_allowlist`) but not change it.

## 11. When you're done

```sh
runner stop demo       # saves its disk
client/aws.sh close    # no SSH from the internet (Tailscale still works)
client/aws.sh stop     # turn the servers off now, or leave them to turn themselves off in an hour
```

## If something's wrong

| You see | Do |
|---|---|
| `AWS won't take this laptop's credentials` | `aws login --profile runner` |
| `runner list` hangs, or can't find `kvm-host` | Connect Tailscale, and check the server is on (`client/aws.sh state`) |
| A server that's off stays off | Rerun `client/install.sh`, which puts `aws.sh` beside `runner`, and `export AWS_PROFILE=runner` |
| `connection to runner-guest closed` right after a restore | Normal: the VM rebooted. Run your command again |
| `demo is busy` | It's running or sleeping: `runner stop demo` first |
| `demo is up, or a run or another runner command holds it` from `rebase` | `runner stop demo` first, or wait for its run to end |
| `base's current generation, … came from "run", not an import` from `rebase` | base booted since its import: import it again (README.md, Build the base image) |
| A VM won't start | `runner logs demo` shows why |
| `kvm-host-2 didn't answer: … Permission denied (publickey)`, from a move or Claude | The servers don't trust each other yet, say because one had an older build when you added the other: `runner hosts add runner@kvm-host-2` again. It names any server without a key: update that one first (README.md, After merging a change) |
| `administratively prohibited` from `runner proxy` | Add `port-forwarding` to the laptop's key line on the server (README.md, Add the laptop's key) |

More detail: README.md.
