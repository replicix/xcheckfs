# First session

In about ten minutes you will mount two scratch directories, watch xcheckfs
mirror your operations, deliberately damage the "experimental" copy, see the
mismatch and the automatic repair, resolve a frozen one, and prove the result
with `verify`. No root
and no real data involved: the two file systems are plain directories in
`/tmp`, and the "experimental" one is wrong only because you make it so.

You need Linux, `fusermount3`, and a built binary:

```bash
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

You will use two terminals, **A** and **B**.

## 1. Prepare two identical trees (terminal A)

```bash
mkdir -p /tmp/xc/p /tmp/xc/s /tmp/xc/m   # primary, secondary, mount point
echo greetings > /tmp/xc/p/hello
rsync -aHAX --numeric-ids /tmp/xc/p/ /tmp/xc/s/
xcheckfs verify /tmp/xc/p /tmp/xc/s
```

The last line prints `... 0 differences, 0 errors` and exits with 0. Never
mount two trees you have not verified: every difference would be reported
from the first operation on.

## 2. Mount with the TUI (terminal A)

```bash
xcheckfs mount --ui tui --quarantine /tmp/xc/q /tmp/xc/m /tmp/xc/p /tmp/xc/s
```

The dashboard opens. The header shows the mount point, `primary → secondary`,
`RUNNING`, check level `basic` and mismatch mode `RESYNC`: the default, which
repairs the secondary when it differs from the primary. `--quarantine` makes
xcheckfs save the secondary's version first, under `/tmp/xc/q`. Press `?` to see the
key bindings and any key to close it again. (The [TUI reference](../reference/tui.md)
describes the panes.)

## 3. Do some operations (terminal B)

```bash
M=/tmp/xc/m
ls -l $M
cat $M/hello
echo more >> $M/hello
mkdir $M/notes
echo "a note" > $M/notes/one.txt
ln $M/notes/one.txt $M/notes/two.txt
mv $M/notes/two.txt $M/notes/three.txt
ls -l $M/notes
```

(Use full paths: a shell whose working directory is inside the mount gets
stuck whenever the file system is frozen, and it prevents unmounting.)

In terminal A the **Operations log** scrolls by: one line per operation with
both results and both latencies, and the **Operations** table counts them.
Everything also exists in the secondary:

```bash
cat /tmp/xc/s/notes/three.txt
xcheckfs verify /tmp/xc/p /tmp/xc/s     # still 0 differences
```

Reading the secondary directly is fine; never *write* to either directory
while xcheckfs runs ([Limitations](../reference/limitations.md#exclusive-access)).

## 4. Corrupt the experimental copy (terminal B)

Wait a couple of seconds (the kernel caches what it saw for one second), then
change one byte of `hello` in the secondary only, behind xcheckfs's back,
and read the file through the mount:

```bash
sleep 2
printf X | dd of=/tmp/xc/s/hello bs=1 seek=0 conv=notrunc
cat /tmp/xc/m/hello
```

`cat` prints the primary's content (`greetings`, `more`): **applications always get
the primary's result**. In terminal A the **Mismatches** pane shows one
entry, and the header counter turns red:

- `lookup attr mtime /hello`: `dd` also changed the secondary's modification
  time, which xcheckfs noticed when the name was looked up. Move to the pane
  with `Tab` and press `Enter` to see everything about a mismatch.

In `resync` mode xcheckfs then repaired the object before replying: the
Counters pane shows `repaired` and `quarantined` at 1, and the Log pane says
where the secondary's version was saved. Look from terminal B:

```bash
cat /tmp/xc/s/hello                      # the primary's content again
cat /tmp/xc/q/*-hello/object             # the damaged copy: Xreetings, more
cat /tmp/xc/q/*-hello/mismatch.txt       # path, reason, the secondary's stat
xcheckfs verify /tmp/xc/p /tmp/xc/s      # 0 differences
```

Read the file through the mount again: no new mismatch, the two copies are
identical. A mismatch is reported once per object, operation, kind and field;
if the same difference struck again, it would be repaired again and counted
as a repeat.

## 5. Freeze and resolve (terminals A and B)

In terminal A press `m` three times: the mode goes `LOG`, `FAIL`, `FREEZE`. In
`freeze` mode a mismatch stops the file system until you decide. Corrupt a
second file, again after a short wait, and read it (terminal B):

```bash
sleep 2
printf Z | dd of=/tmp/xc/s/notes/one.txt bs=1 seek=0 conv=notrunc
cat /tmp/xc/m/notes/one.txt
```

`cat` hangs. In terminal A a red **FROZEN** modal shows the mismatch and the
possible decisions. Press `s` (**resync**): xcheckfs repairs the secondary's
copy of that object from the primary, which is what `resync` mode does by
itself. In terminal B `cat` returns `a note`, and the header is back to
`RUNNING`. Other keys: `c` continue, `a` allow, `r` retry, `e` fail with
`EIO`, `d` detach; see [Handle a frozen mismatch](../how-to-guides/handle-a-frozen-mismatch.md).

You could have decided from terminal B as well:

```bash
xcheckfs ctl /tmp/xc/m status           # "state": "frozen"
xcheckfs ctl /tmp/xc/m pending          # note the "id" of the mismatch
xcheckfs ctl /tmp/xc/m resolve ID resync
```

## 6. Unmount and verify

In terminal B:

```bash
fusermount3 -u /tmp/xc/m
```

The TUI in terminal A closes and prints a summary line. Check the exit status
there:

```bash
echo $?        # 3: at least one mismatch was recorded
```

Now compare the trees:

```bash
xcheckfs verify /tmp/xc/p /tmp/xc/s
```

Both files are clean: you repaired them, and the damaged versions are in
`/tmp/xc/q` (`ls /tmp/xc/q`). With `--on-mismatch log` the mismatch would
only have been reported, and `verify` would still list `/hello`.

## Clean up

```bash
rm -rf /tmp/xc
```

## Where next

- Test a real file system: [Test an experimental file system](../how-to-guides/test-an-experimental-fs.md)
- Gate a CI job on it: [Run in CI](../how-to-guides/run-in-ci.md)
- Understand why it can compare under concurrency: [Design](../explanation/DESIGN.md)
