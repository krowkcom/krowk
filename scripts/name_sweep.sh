#!/usr/bin/env bash
# R-REN-13: the product is gruntas. Every line of the repo that still says
# krowk (`git grep -i krowk`) has to be on scripts/name_sweep.allow, which
# says why: an identifier existing data, signatures or other machines
# depend on (D9), a krowk.com host kept until the domain move, the one-time
# copy of ~/.krowk, history. Anything else fails `make check`.
#
# An allow entry is a regex taken out of each matching line, or a whole
# file; a line passes only when nothing spelled krowk is left, so a stray
# one beside an allowed name is still caught. Before the repo, the script
# checks itself on lines it makes up: an allowed one passes, a stray one
# fails.
set -euo pipefail
export LC_ALL=C
cd "$(dirname "$0")/.."
allow=scripts/name_sweep.allow

# Reads `path NUL line NUL text` records (git grep -z -n) on stdin and
# prints the ones the allowlist does not cover. Exits 1 when it printed any.
check() {
  perl -e '
    use strict; use warnings;
    my ($allow) = @ARGV;
    my (@files, @everywhere, @scoped);
    sub glob_re {
      my $g = shift; my $r = "";
      while ($g =~ /\G(\*\*|\*|\?|[^*?]+)/gc) {
        $r .= $1 eq "**" ? ".*" : $1 eq "*" ? "[^/]*" : $1 eq "?" ? "[^/]" : quotemeta $1;
      }
      qr/\A$r\z/;
    }
    open my $fh, "<", $allow or die "name_sweep: cannot read $allow: $!\n";
    while (my $l = <$fh>) {
      chomp $l;
      next if $l =~ /^\s*(#|$)/;
      if ($l =~ /^file:\s*(\S+)\s*$/) { push @files, glob_re($1) }
      elsif ($l =~ /^in\s+(\S+)\s*::\s*(.+?)\s*$/) { push @scoped, [glob_re($1), qr/$2/] }
      else { $l =~ s/^\s+|\s+$//g; push @everywhere, qr/$l/ }
    }
    local $/ = "\n";
    my $bad = 0;
    while (my $rec = <STDIN>) {
      chomp $rec;
      my ($path, $n, $text) = split /\0/, $rec, 3;
      next if grep { $path =~ $_ } @files;
      my $left = $text;
      $left =~ s/$_//g for @everywhere;
      for (@scoped) { my ($p, $r) = @$_; $left =~ s/$r//g if $path =~ $p }
      next unless $left =~ /krowk/i;
      print "$path:$n: $text\n";
      $bad = 1;
    }
    exit $bad;
  ' "$allow"
}

# The check on lines made up for it: a pinned label, then the same label
# with a stray name after it.
selftest_ok=$(printf 'x.rs\000%s\000%s\n' 1 'const L: &[u8] = b"krowk/pair/v1";' | check) || {
  echo "name_sweep: self-test: an allowed line was refused: $selftest_ok" >&2
  exit 1
}
if printf 'x.rs\000%s\000%s\n' 1 'const L: &[u8] = b"krowk/pair/v1"; // krowk' | check >/dev/null; then
  echo "name_sweep: self-test: a stray krowk was let through" >&2
  exit 1
fi

if ! out=$(git grep -z -n -I -i krowk | check); then
  printf '%s\n' "$out" >&2
  echo "name_sweep: the lines above say krowk. Rename them to gruntas, or, if the name is one data or other machines depend on, add it to $allow with the reason." >&2
  exit 1
fi
echo "name_sweep: every krowk left is on $allow"
