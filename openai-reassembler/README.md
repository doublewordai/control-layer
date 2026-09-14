
# Reassemble OpenAI

This crate is maintained in the control-layer Rust workspace and consumed by
`dwctl` and `fusillade` through path dependencies. It is not published separately.
The initial import preserves version 0.4.0 and its existing fixture suite.
A package to take completion and chat_completion SSEs and reconstruct them back into a non-streamed equivalent.

## Testing
`cargo test -p openai-reassembler` will run the tests with existing fixtures generated using vLLM with this configuration:

```
docker run -d --name vllm-cpu --cpus 4 -p 8000:8000 -v ~/.cache/huggingface:/root/.cache/huggingface vllm/vllm-openai-cpu:latest --model Qwen/Qwen2-0.5B-Instruct --dtype float32 --max-model-len 128 --max-num-seqs 4 --enable-auto-tool-choice --tool-call-parser hermes
```

The tests use a deterministic request to get a streamed and non-streamed response. We then run the reassembly algorithm on the streamed version and compare it to the non-streamed version.

You can regenerate the fixtures by running `BASE_URL=http://xxxxx.xxxx/v1 FIXTURE_NAME=xxxx MODEL=xxxx cargo test -p openai-reassembler`
Fixture name is the type of provider you want to test against – fixtures will go into a folder of that name. For example you might want to test against both vllm and dynamo, so you'd set it to e.g. vllm_except_the_name_is_arbitrary and dynamo_fish for each fixture generation run.