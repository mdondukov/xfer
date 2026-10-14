# xfer

A command-line tool for moving documents, photos and videos from an old Windows PC to a new one,
and for cleaning duplicates out of an archive.

| Command | What it does |
|---|---|
| `xfer copy` | copies chosen drives and folders, as is or with duplicates skipped |
| `xfer dupes` | finds duplicates without copying and builds a report you mark up |
| `xfer apply` | moves the duplicates you marked to a quarantine folder |

Nothing is ever deleted outright: `copy` only reads the sources, and `apply` moves files to
quarantine, where you can restore them.

## Scenarios

### 1. Move everything useful to a new PC through an external drive

The main scenario: the old PC has hundreds of GB scattered across folders, with junk and
duplicates mixed in. You want one clean copy of every document, photo and video on the new PC.

**On the old PC**, preview first:

```
xfer copy C:\Users\Max D:\ --to E:\transfer --dedup --only-useful --dry-run
```

This copies nothing. It prints a summary:

```
Sources:
  C:\Users\Max
  D:\
Disks:
  C:\          source  SSD, physical disk 0
  D:\          source  HDD, physical disk 1
  E:\          target  HDD, physical disk 2
Copy mode: pipeline (2 readers, one per source disk, 1 writer, sequential access to each disk; chosen automatically)
Found: 182,410 files, 412.6 GB
Filtered out (not whitelisted): 96,203 files, 38.1 GB
  .exe                    1,204 files      9.8 GB
  .dll                   31,877 files      6.2 GB
  ...
Skipped system folders: 14
  C:\Users\Max\AppData
  ...
By category:
  Video                   2,310 files    251.4 GB
  Photos                 61,880 files    121.0 GB
  Documents              20,017 files      2.9 GB
Duplicates:
  Groups of identical files: 24,113
  Extra copies: 41,580 files, 163.2 GB
  Space saved: 163.2 GB (40% of total)
  Largest duplicates:
    3 copies x    4.1 GB  D:\Video\wedding.mp4
  Folders whose entire content exists elsewhere: 37
     58.3 GB  D:\Backup 2019
               -> fully contained: D:\Photos
To copy: 140,830 files, 249.4 GB -> E:\transfer
```

The dry run leaves the target untouched and saves its reports in `xfer-dryrun-<time>` in the
current folder:

- `plan.csv` - every file that would be copied and where, plus every duplicate that would be
  skipped and which file it duplicates;
- `filtered.csv` - every file the filter left out, to spot useful extensions missing from the
  whitelist;
- `errors.txt` - files that could not be read;
- `summary.txt` - the summary above.

Check that the numbers make sense, then run the same command without `--dry-run`:

```
xfer copy C:\Users\Max D:\ --to E:\transfer --dedup --only-useful
```

The external drive keeps full paths with the drive letter, plus a log of the run:

```
E:\transfer\C\Users\Max\Documents\...
E:\transfer\D\Photos\...
E:\transfer\_xfer\<time>\manifest.csv   where every file went, and which file each duplicate matches
E:\transfer\_xfer\<time>\summary.txt    the summary above
E:\transfer\_xfer\<time>\filtered.csv   every file that was filtered out
E:\transfer\_xfer\<time>\errors.txt     files that could not be read or copied
```

**On the new PC**:

```
xfer copy E:\transfer
```

Files land in `C:\transfer` by default; pass `--to` for another folder. xfer recognizes a
transfer folder by its `_xfer` subfolder and copies its content without an extra level, so you
get `C:\transfer\C\Users\Max\...`, not `C:\transfer\E\transfer\C\...`.

Tips:

- Before copying, xfer checks for real-time antivirus scanning and, if it is on, prints the
  commands to exclude `xfer.exe` for the duration of the transfer (see [Antivirus](#antivirus)).
- Format the external drive as NTFS, not exFAT.

### 2. Copy folders exactly as they are

For a project folder or anything where structure matters more than space:

```
xfer copy "D:\Work\Project X" --to E:\transfer
```

Without `--dedup` and `--only-useful`, every file is copied. Add `--exclude-ext iso,tmp` to drop
specific extensions.

### 3. Stop and resume a copy

Press Ctrl+C once: xfer finishes the small files in progress, stops large files mid-way, and
still writes the manifest and summary. Files not copied yet are listed as
`not-copied (interrupted)`. Press Ctrl+C a second time to quit immediately.

Ctrl+C while hashing (with `--dedup` or in `xfer dupes`) works the same way: hashes computed so
far are saved to the cache every few seconds, and the next run reuses them instead of reading
those files again.

To continue, run the same command again. Files already in place (same size and modification time) are
skipped, and the summary shows how much was copied earlier. A file that was cut off mid-copy is
left as `<name>.xfer-partial` and gets overwritten.

### 4. Decide which copy to keep

With `--dedup`, xfer keeps one copy of each file. It picks, in order:

1. a copy under a `--prefer` folder;
2. a copy outside a duplicate folder, so an old backup drops out as a whole;
3. the copy with the shortest path;
4. the oldest copy.

If you have a "main" archive and a pile of copies from colleagues, point xfer at the archive:

```
xfer copy D:\ --to E:\transfer --dedup --prefer D:\Archive
```

`--prefer` can be repeated.

### 5. Clean duplicates out of a drive

To shrink an archive in place, for example on the external drive after the move:

```
xfer dupes E:\
```

This creates a folder `xfer-dupes-<time>` in the current directory:

- `report.html` - an overview to browse: summary, duplicate folders, groups of identical files;
- `folders.csv` - folders whose entire content exists elsewhere;
- `files.csv` - every group of identical files.

Open the CSV files in Excel and put `x` in the `delete` column next to what you want removed.
The `suggest` column helps: `delete` or `keep` for folders, `keep` or `duplicate` for files.
Marking a folder marks all of its duplicate files. Save as **CSV UTF-8**.

Preview, then apply:

```
xfer apply xfer-dupes-<time> --dry-run
xfer apply xfer-dupes-<time>
```

Marked files move to `<scanned folder>\_dupes_trash\<time>\` with their paths preserved, and
folders that end up empty are removed. Safeguards:

- every group keeps at least one copy, even if you marked all of them;
- before moving anything, xfer verifies that the remaining copy exists and has the same content;
- a file that changed after the report is left alone.

Run `xfer dupes` again to confirm the result. When you are sure, delete the `_dupes_trash`
folder to free the space.

### 6. Restore something from quarantine

`_dupes_trash\<time>\moved.csv` lists the original and quarantine path of every moved file.
Move the file back by hand.

### 7. Tune the whitelist

Run with `--dry-run` and look at the "Filtered out" section and at `filtered.csv` in the
`xfer-dryrun-<time>` report folder. If something useful is there, add its extension:

```
xfer copy D:\ --to E:\transfer --only-useful --include-ext 1cd,dt
```

## What counts as useful (`--only-useful`)

| Category | Extensions |
|---|---|
| Documents | pdf, doc(x), rtf, odt, txt, md, xls(x), ods, csv, ppt(x), pps(m), odp, vsd(x), pub, xps, one, pages, numbers, key, htm(l), mht, sig, p7s, wbk; databases and reports accdb, mdb, pbix, twb(x), mpp; mind maps mm, xmind... |
| Books | djvu, epub, fb2, mobi, azw3, chm, cbr, cbz |
| Photos | jpg, png, gif, bmp, tif, webp, heic, avif, svg, camera RAW (dng, cr2, cr3, nef, arw, orf, rw2, raf...), xmp |
| Video | mp4, mov, avi, mkv, wmv, mpg, ts, mts, m2ts, vob, 3gp, webm... and subtitles srt, ass, sub, vtt |
| Audio | mp3, m4a, aac, wav, flac, ogg, opus, wma, aiff, amr, ape, mid |
| Mail & contacts | pst, eml, msg, mbox, vcf, ics, contact |
| Design & CAD | psd, ai, eps, indd, cdr, xcf, sketch, fig, dwg, dxf, skp |
| Archives | zip, rar, 7z, tar, gz, bz2, xz |

The full list is in `src/filter.rs`. ISO images and 1C databases are not included; add them with
`--include-ext` if you need them.

With `--only-useful`, xfer does not descend into `AppData`, `node_modules` or `.git`, nor into
system folders at a drive root: `Windows`, `Program Files`, `ProgramData`, `Recovery` and similar.
The recycle bin and `System Volume Information` are always skipped. OneDrive cloud-only files are
skipped too, so they don't trigger a download; the summary counts them separately.

## Progress

Every command runs as numbered stages. Each stage shows a bar with speed and time left, and
leaves a one-line result when done:

```
  [1/5] Scanning     done: 182,410 files, 412.6 GB (0:21)
  [2/5] Quick check  done: 64,800 files with a same-size twin, 31,200 need a full hash (0:12)
  [3/5] Hashing      done: 96.0 GB hashed, 0 from cache (3:05)
  [4/5] Folders      done: 24,113 duplicate groups, 37 duplicate folders (0:02)
  [5/5] Copying      [=========>          ] 98.3 GiB/249.4 GiB 185 MiB/s (13:40 left)
      52,301/140,830 files, 0 errors
      > wedding.mp4 2.1 GiB/4.1 GiB
      > trip-2019.mov 0.4 GiB/1.8 GiB
```

Scanning has no ETA because the total is unknown until it finishes; it shows the count and rate
instead. Progress goes to stderr, so `xfer copy ... > summary.txt` captures a clean summary.

## Speed: hard drives, SSDs and copy modes

xfer detects whether each source and the target are SSDs or hard drives, and prints it:

```
Disks:
  D:\          source  HDD, physical disk 1
  F:\          target  HDD, physical disk 2
Copy mode: pipeline (1 reader, 1 writer, sequential access to each disk; chosen automatically)
```

A hard drive has a single head. Several threads reading or writing the same HDD make it seek
back and forth, and the total gets slower than one sequential stream. Different disks, on the
other hand, don't get in each other's way. Hence two copy modes:

| Mode | Used when | How it works |
|---|---|---|
| `pipeline` | any HDD is involved, or the disk type is unknown | One reader per source disk walks files in folder order (neighbouring files tend to sit close on the platter); one writer writes them in order. A queue of up to ~256 MB between them lets reading the next file overlap with writing the current one, so the total time approaches that of the slower disk rather than the sum of both. |
| `parallel` | all disks are SSDs | Two lanes side by side: files under 16 MB in 8 threads (`--threads`), since they are bound by per-file overhead; larger files in 2 threads (`--large-threads`), largest first, since they are bound by bandwidth. |

`--copy-mode pipeline` or `--copy-mode parallel` overrides the automatic choice. The type is
read from the Windows "seek penalty" property, the same one the defragmenter uses, without admin
rights. USB enclosures often hide it; such disks show as "unknown type, treated as HDD", because
sequential access costs an SSD little while parallel access hurts an HDD a lot.

The same rule applies elsewhere: sources on different physical disks are scanned in parallel,
and with `--dedup` each hard drive is hashed by a single thread (SSDs use `--jobs` threads).

On hard drives xfer also:

- reads files in their on-disk order rather than folder order: a `Locating` stage asks Windows
  where each file starts on the platter (the same query the defragmenter uses), so the head
  sweeps across the disk once instead of jumping between folders;
- hashes same-size files under 4 MB in full right away: the quick head-and-tail check costs two
  seeks, about as much as reading such a file outright;
- opens files with the sequential-read hint, so Windows reads ahead in large blocks.

Large files are written into a preallocated target file, which limits fragmentation.

Rough expectations for a portable USB hard drive: large files at 80-120 MB/s, small files at a
few hundred per second. No setting gets past the slowest disk, which is why `--only-useful` and
`--dedup` are the biggest speed-ups: they cut what has to be copied at all.

To compare settings on your hardware, copy a 10-20 GB sample in both modes:

```
xfer copy D:\Sample --to F:\test1 --copy-mode pipeline
xfer copy D:\Sample --to F:\test2 --copy-mode parallel
```

## Antivirus

Real-time scanning checks every file xfer reads and writes, which noticeably slows down copying
many small files. Before copying, xfer asks Windows whether Microsoft Defender real-time
protection or another antivirus is on, and if so prints something like:

```
Note: Microsoft Defender real-time protection is on. It scans every file xfer
reads and writes, which noticeably slows down copying many small files.
To exclude xfer for the duration of the transfer, run in an admin PowerShell:
  Add-MpPreference -ExclusionProcess "C:\Tools\xfer.exe"
and remove the exclusion when done:
  Remove-MpPreference -ExclusionProcess "C:\Tools\xfer.exe"
```

xfer never changes security settings itself. On managed PCs policy would block it anyway, and an
exclusion left behind by an interrupted run would stay forever. A process exclusion is
suggested rather than a folder one because it covers both reading on the old PC and writing to
the external drive. For other antivirus products, add `xfer.exe` to exclusions in their
settings. Skip the check with `--no-av-check`.

## Options

| Option | Commands | Meaning |
|---|---|---|
| `--to <folder>` | copy | target folder, default `C:\transfer` |
| `--dedup` | copy | copy each file once |
| `--prefer <folder>` | copy, dupes | keep copies from this folder |
| `--only-useful` | copy, dupes | whitelisted files only |
| `--include-ext a,b` | copy, dupes | add extensions to the whitelist |
| `--exclude-ext a,b` | copy, dupes | always skip these extensions |
| `--dry-run` | copy, apply | show what would happen, change nothing |
| `--relative` | copy | put the source content directly into the target, without the drive letter |
| `--copy-mode <m>` | copy | `auto` (default), `pipeline` or `parallel`, see [Speed](#speed-hard-drives-ssds-and-copy-modes) |
| `--threads <N>` | copy | parallel mode: copies of small files at once, default 8 |
| `--large-threads <N>` | copy | parallel mode: copies of large files at once, default 2 |
| `--no-av-check` | copy | skip the antivirus check |
| `--out <folder>` | dupes | where to write the report |
| `-y, --yes` | apply | don't ask for confirmation |
| `-j, --jobs <N>` | all | threads for hashing files on SSDs (default: CPU cores); hard drives always get one |
| `--no-cache` | all | don't use the hash cache |

## How duplicates are found

1. Files are grouped by size. A file with a unique size can't be a duplicate and is never read.
2. Within a size group, the first and last 16 KB are hashed.
3. Only files that still match are hashed in full (BLAKE3).

Full hashes are cached in `%LOCALAPPDATA%\xfer\hash-cache.redb`, so repeated runs don't re-read
unchanged files.

A folder is reported as a duplicate when every file inside it has a copy somewhere outside it.
The report says where that content lives: an exact copy, a larger folder that contains it, or
several places.

## Getting the binary

Every push to `main` and every pull request builds and tests xfer on Windows in GitHub Actions.
Download `xfer-windows-x64.exe` from the run page under **Artifacts**; it stays there for 30 days.

To publish a release, push a tag:

```
git tag v0.1.0
git push origin v0.1.0
```

The exe is attached to a GitHub Release for that tag. You can also start a build by hand from the
**Actions** tab (**Run workflow**).

The exe is statically linked and needs no Visual C++ Redistributable.

Local builds:

```
cargo build --release                                  # on Windows
cargo build --release --target x86_64-pc-windows-gnu   # on macOS, needs mingw-w64
```

## Notes

Explorer shows some folders under translated names that don't exist on disk: on a Russian
Windows "Пользователи" is really `Users`, "Документы" is `Documents`. Click Explorer's address
bar or use Shift+right-click > "Copy as path" to get the real path.

In `cmd`, don't end a quoted path with a backslash: `"D:\Photos\"` reaches the program as
`D:\Photos"`. xfer repairs this, but `"D:\Photos"` is safer.

## License

MIT, see [LICENSE](LICENSE).
