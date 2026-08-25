fromjson?
| select((.fields.error? // "") | contains("Verifier output"))
| (
    (.fields.monitor // .message),
    (.fields.error | split("\n") | .[-8:] | join("\n")),
    ""
  )
