def build-document [out: string, row: list<string>] {
  if ($row | length) != 3 {
    error make { msg: $"invalid workspace row: ($row)" }
  }

  let name = $row.0
  let state = $row.1
  let source = $row.2
  if not ($source | path exists) {
    error make { msg: $"missing document: ($source)" }
  }

  let destination = match $state {
    "published" => $"published/($name).md"
    "draft" => $"drafts/($name).md"
    _ => { error make { msg: $"invalid state for ($name): ($state)" } }
  }
  cp $source ($out | path join $destination)
  let digest = (open --raw $source | hash sha256)
  $"($name)\t($state)\t($destination)\t($digest)\n" | save --raw --append ($out | path join index.tsv)
}

def main [out: string = ".work/nu"] {
  cd $env.FILE_PWD
  if ($out | path exists) { rm -rf $out }
  mkdir ($out | path join published) ($out | path join drafts)

  open --raw workspace.tsv
  | lines
  | where { |line| ($line | str trim | is-not-empty) }
  | each { |line| build-document $out ($line | split row "\t") }
  | ignore
}

