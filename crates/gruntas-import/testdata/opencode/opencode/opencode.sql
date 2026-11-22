-- Fixture database for the opencode importer tests, in the minimal
-- schema the reader selects: project/session/message/part with the id,
-- time and data columns it queries. Never a binary .db in git: the test
-- builds a temp database by executing this file with {{WORKTREE}}
-- replaced by a real directory, then points the Source at that home.
--
-- What it covers, by hand for the tests to hold against:
--   parent + child sessions (parent_id link, worktree path check),
--   user + assistant messages with tokens and dollar costs,
--   text, reasoning, tool completed (twins), tool error (twins),
--   tool running (single), file, patch, step-start and step-finish parts.
CREATE TABLE project (
  id TEXT PRIMARY KEY,
  worktree TEXT NOT NULL,
  vcs TEXT
);
CREATE TABLE session (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  parent_id TEXT,
  directory TEXT NOT NULL,
  title TEXT NOT NULL,
  model TEXT,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL
);
CREATE TABLE message (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  data TEXT NOT NULL
);
CREATE TABLE part (
  id TEXT PRIMARY KEY,
  message_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  data TEXT NOT NULL
);
INSERT INTO project (id, worktree, vcs) VALUES ('prj_1', '{{WORKTREE}}', 'git');
INSERT INTO session (id, project_id, parent_id, directory, title, model, time_created, time_updated) VALUES
  ('ses_parent', 'prj_1', NULL, '{{WORKTREE}}', 'Parent session', '{"id":"gpt-5.5","providerID":"openai"}', 1757000000000, 1757000000900),
  ('ses_child', 'prj_1', 'ses_parent', '{{WORKTREE}}', 'Child session', '{"id":"gpt-5.5","providerID":"openai"}', 1757000001000, 1757000001500);
INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES
  ('msg_u1', 'ses_parent', 1757000000100, 1757000000101, '{"role":"user"}'),
  ('msg_a1', 'ses_parent', 1757000000200, 1757000000201, '{"role":"assistant","modelID":"gpt-5.5","providerID":"openai","tokens":{"input":9121,"output":1660,"reasoning":23,"cache":{"read":42496,"write":120}},"cost":0.012345,"finish":"stop"}'),
  ('msg_u2', 'ses_parent', 1757000000300, 1757000000301, '{"role":"user"}'),
  ('msg_a2', 'ses_parent', 1757000000400, 1757000000405, '{"role":"assistant","modelID":"gpt-5.5","providerID":"openai","tokens":{"input":100,"output":50,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0.0001,"finish":"stop"}'),
  ('msg_c_u1', 'ses_child', 1757000001100, 1757000001101, '{"role":"user"}'),
  ('msg_c_a1', 'ses_child', 1757000001200, 1757000001205, '{"role":"assistant","modelID":"gpt-5.5","providerID":"openai","tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0.00001,"finish":"stop"}');
INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES
  ('prt_u1_text', 'msg_u1', 'ses_parent', 1757000000100, 1757000000101, '{"type":"text","text":"first prompt"}'),
  ('prt_a1_text', 'msg_a1', 'ses_parent', 1757000000200, 1757000000200, '{"type":"text","text":"Let me look at that."}'),
  ('prt_a1_reason', 'msg_a1', 'ses_parent', 1757000000201, 1757000000201, '{"type":"reasoning","text":"Need to read the file first."}'),
  ('prt_a1_tool1', 'msg_a1', 'ses_parent', 1757000000202, 1757000000202, '{"type":"tool","tool":"read","callID":"call_1","state":{"status":"completed","input":{"file":"a.ts"},"output":"const x = 1"}}'),
  ('prt_a1_tool2', 'msg_a1', 'ses_parent', 1757000000203, 1757000000203, '{"type":"tool","tool":"bash","callID":"call_2","state":{"status":"error","input":{"command":"exit 1"},"output":"boom"}}'),
  ('prt_a1_tool3', 'msg_a1', 'ses_parent', 1757000000204, 1757000000204, '{"type":"tool","tool":"grep","callID":"call_3","state":{"status":"running","input":{"pattern":"foo"}}}'),
  ('prt_a1_file', 'msg_a1', 'ses_parent', 1757000000205, 1757000000205, '{"type":"file","name":"notes.txt","mime":"text/plain"}'),
  ('prt_a1_patch', 'msg_a1', 'ses_parent', 1757000000206, 1757000000206, '{"type":"patch","diff":"--- a/a.ts\n+++ b/a.ts"}'),
  ('prt_a1_step1', 'msg_a1', 'ses_parent', 1757000000207, 1757000000207, '{"type":"step-start","snapshot":"abc123"}'),
  ('prt_a1_step2', 'msg_a1', 'ses_parent', 1757000000208, 1757000000208, '{"type":"step-finish","reason":"stop","snapshot":"def456"}'),
  ('prt_u2_text', 'msg_u2', 'ses_parent', 1757000000300, 1757000000301, '{"type":"text","text":"second prompt"}'),
  ('prt_a2_text', 'msg_a2', 'ses_parent', 1757000000400, 1757000000405, '{"type":"text","text":"Done."}'),
  ('prt_cu1_text', 'msg_c_u1', 'ses_child', 1757000001100, 1757000001101, '{"type":"text","text":"child prompt"}'),
  ('prt_ca1_text', 'msg_c_a1', 'ses_child', 1757000001200, 1757000001205, '{"type":"text","text":"child answer"}');
