-- Admin review of uploaded plugins (docs/plugins.md §14.6, docs/api.md
-- "用户上传的插件"). `review`: what plugin-pack's report step delivered for
-- reading (file list, Dockerfile, small text files), JSON. `approval`: who
-- made the plugin public after checking the list, when, and the note, JSON.
ALTER TABLE user_plugins ADD COLUMN review TEXT;
ALTER TABLE user_plugins ADD COLUMN approval TEXT;
