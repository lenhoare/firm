# Rough Spec

## Basic idea

I dont have much money so have to rely on my basic ChatGPT subscription
I often get freebies / good discounts from other AI providers

1. Codex runs as a manager agent (the codex sdk is newly released.)
2. It does its best to budget and minimise its usage so that it just avoids hitting limits (and therefore maximising the amount of credits I use)
It also does the most important thinking / design work for a project. Im guessing it does this by separating short commands to subagents and long creative work.  and then only doing longer creative work every x minuts based on the 5 hour limit to start with.

Ive heard online that "The Codex backend returns rate-limit snapshots containing things like the 5-hour window used percentage, weekly used percentage, reset times, and credit balance. People have confirmed these values are present in Codex session rollouts and in the internal backend-api/codex/usage/wham/usage responses"
3. Codex delegates easier work to my cheaper agents.  Again though we will need to have a mechanism for budget limiting these with their own config.
Although these agents are no where as powerful as you, they are suprisingly good.
They are accessed through their CLI tools, so we will need to use a pty to access them. i built something before in rust and a few learnings from this project may make us lean towards python rather than rust (see prior_learning.md)
lets start with qwen, muse and pi (I can connect pi to several models in openrouter or merge where I have free tokens to use)

4. The whole budgetting process will be a feedback loop and as such will be a matter of trial and error to start with but Im hoping after a few weeks of work we can get the values in config to acheive the balance required.
Im thinking I probably would like controls so I can ask to whack a load of work in a certain providers direction based on the amount of credits I have left and the date.

5. Things I want to be able to do / see: i guess I want it to look and feel exactly like codex where I talk to you, the manager, but with a separate window or something and an option to see what is going on with subagents.


Something else to consider and include:  the fact that manager and subagents is a good thing to do anyway, even if it was just with codex:

## A tweet for a very successful project that created a blender version of manhattan using Astra:
1. Launch an agent (I'm calling this one the "manager"). Chat with it about what you want to get done, and have it build a massive checklist of to-dos, then break that checklist into phases.

2. The manager then spawns a second agent in a separate thread (the "implementer"). The two agents can message each other.

3. Put the manager in /goal mode, and tell it to run each phase on the implementer in /goal mode.

4. The manager messages the implementer: "/goal Complete phase one completely, extremely well." The implementer doesn't stop until that phase is done, then messages the manager back. The manager tells it to start phase two. They repeat until every phase is finished, completely autonomously.

Why I think this works: over a long-horizon task, Astra tends to asymptote. It gets way further than previous models, but at a certain point it kind of just stops improving against the goal as quickly as it did before. It gets stuck in the minutiae, focusing way too much on small details, and overall progress stalls. The Manager Loop forces it to work piecemeal, one phase at a time. It's essentially how a human would steer a model, except the model is doing the steering for me.

That's actually how this started. I was having the model write the checklist and break it into phases, and then I was doing the manager's job by hand. At some point I thought, "Wait, why can't I just get a separate AI to do this?" That's what unlocked full autonomy, which is super useful.

A wording detail that seemed to matter: I ask for each phase to be done "extremely well," not "perfectly." Maybe I'm reading too much into it, but asking for "perfect" sent the model right back into the minutiae. "Extremely well" implies it's allowed to move on once it's good enough, and that worked better in my testing.

One more trick that I think helps (this one is more of a hunch, but it was useful for me): have the implementer build a simple HTML page with the full checklist on it. The implementer checks boxes off as it goes and updates a counter, and the page has a chart of # of boxes ticked over time.

Obviously the boxes aren't all equal, but it forces the model to notice things like "I haven't made progress in a while, time to move on." You can even put this in the prompt directly, like: "if you haven't ticked a box in X amount of time, move on". That helps a lot.

I also ran 96 sub-agents at a time. You can change this in your Codex config (or just ask Codex to change it).

This got me far better long-horizon performance than anything else I tried. I'll be sharing more in the coming days!

## Future directions:
Be able to contact me and receive commands from my phone, maybe through a web interface, app or something like telegram.
Maybe resurrect my quest 2 with an interface for that so I can orchestrate while in the garden in VR (very long term)
