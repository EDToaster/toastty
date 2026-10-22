#!/usr/bin/env python3
"""Kitty animation conformance probe. Run INSIDE the terminal under test.

usage: anim-probe.py OUT.jsonl SHOTDIR [case-prefix...]   (env SETTLE=secs)

For every case: clears the screen, frees all images, sends the case's
graphics commands, then a DA1 query as a sentinel, and records every
reply that arrived before the DA1 answer. Cases with `shot` set ask the
outer driver for a screenshot (SHOTDIR/req -> SHOTDIR/ack handshake)
after placing the image at the top-left.
"""
import base64, json, os, select, sys, termios, time, tty, zlib

OUT = sys.argv[1]
SHOTDIR = sys.argv[2]
ONLY = sys.argv[3:]

R = (255, 0, 0, 255)
G = (0, 255, 0, 255)
B = (0, 0, 255, 255)
W = (255, 255, 255, 255)
K = (0, 0, 0, 255)
T = (0, 0, 0, 0)
HR = (255, 0, 0, 128)   # half red
HG = (0, 255, 0, 128)   # half green
HB = (0, 0, 255, 128)


def px(*cols):
    return bytes(c for col in cols for c in col)


def solid(col, w=4, h=2):
    return px(*([col] * (w * h)))


def b64(b):
    return base64.standard_b64encode(b).decode()


def apc(keys, payload=b""):
    s = "\x1b_G" + keys
    if payload:
        s += ";" + b64(payload)
    return s + "\x1b\\"


def root(i=1, col=R, w=4, h=2, extra=""):
    return apc(f"a=t,f=32,s={w},v={h},i={i}{extra}", solid(col, w, h))


def frame(keys, data):
    return apc("a=f," + keys, data)


def place(i=1, extra="c=8,r=2"):
    # Place at the home cell, cursor not moved.
    return "\x1b[H" + apc(f"a=p,i={i},q=2,C=1,{extra}")


CASES = []


def case(name, cmds, shot=False, settle=0.0):
    CASES.append({"name": name, "cmds": cmds, "shot": shot, "settle": settle})


# ---- a=f replies ----
case("f_new_frame_reply", [root(), frame("i=1,f=32,s=4,v=2", solid(G)),
                           frame("i=1,f=32,s=4,v=2", solid(B))])
case("f_edit_reply", [root(), frame("i=1,f=32,s=4,v=2", solid(G)),
                      frame("i=1,r=2,f=32,s=4,v=2", solid(B)),
                      frame("i=1,r=1,f=32,s=4,v=2", solid(B))])
case("f_edit_out_of_range", [root(), frame("i=1,r=5,f=32,s=4,v=2", solid(G)),
                             frame("i=1,r=3,f=32,s=4,v=2", solid(G))])
case("f_base_missing", [root(), frame("i=1,c=3,f=32,s=4,v=2", solid(G)),
                        frame("i=1,c=1,f=32,s=4,v=2", solid(G)),
                        frame("i=1,c=2,f=32,s=4,v=2", solid(G))])
case("f_too_wide", [root(), frame("i=1,f=32,s=5,v=2", solid(G, 5, 2)),
                    frame("i=1,f=32,s=4,v=3", solid(G, 4, 3)),
                    frame("i=1,f=32,s=1,v=1,x=9,y=9", solid(G, 1, 1))])
case("f_no_data", [root(), frame("i=1,f=32,s=4,v=2", b""), apc("a=f,i=1")])
case("f_no_dims", [root(), frame("i=1,f=32", solid(G)), frame("i=1", solid(G))])
case("f_short_data", [root(), frame("i=1,f=32,s=4,v=2", solid(G)[:10])])
case("f_by_number", [root(i=0, extra=",I=7"), frame("I=7,f=32,s=4,v=2", solid(G))])
case("f_quiet", [root(), frame("i=1,q=1,f=32,s=4,v=2", solid(G)),
                 frame("i=1,q=1,c=9,f=32,s=4,v=2", solid(G)),
                 frame("i=1,q=2,c=9,f=32,s=4,v=2", solid(G))])
case("f_unknown_image", [frame("i=42,f=32,s=4,v=2", solid(G)),
                         frame("I=42,f=32,s=4,v=2", solid(G)),
                         frame("f=32,s=4,v=2", solid(G))])
case("f_placement_id", [root(), frame("i=1,p=5,f=32,s=4,v=2", solid(G))])
case("f_gap_keys", [root(), frame("i=1,z=-5,f=32,s=4,v=2", solid(G)),
                    frame("i=1,z=0,f=32,s=4,v=2", solid(G)),
                    frame("i=1,z=100,f=32,s=4,v=2", solid(G))])
PNG_G = None
try:
    import io
    from PIL import Image
    im = Image.new("RGBA", (4, 2), G)
    bio = io.BytesIO(); im.save(bio, "PNG"); PNG_G = bio.getvalue()
    im = Image.new("RGBA", (5, 2), G)
    bio = io.BytesIO(); im.save(bio, "PNG"); PNG_G5 = bio.getvalue()
except Exception:
    PNG_G = None
if PNG_G:
    case("f_png", [root(), frame("i=1,f=100", PNG_G), frame("i=1,f=100", PNG_G5),
                   frame("i=1,f=100", b"notapng")])
case("f_zlib", [root(), frame("i=1,f=32,s=4,v=2,o=z", zlib.compress(solid(G))),
                frame("i=1,f=32,s=4,v=2,o=z", b"garbage")])


def chunked(keys, data, n=8):
    enc = b64(data)
    parts = [enc[i:i + n] for i in range(0, len(enc), n)]
    out = []
    for k, p in enumerate(parts):
        more = 1 if k < len(parts) - 1 else 0
        hdr = (f"a=f,{keys},m={more}" if k == 0 else f"a=f,m={more}")
        out.append("\x1b_G" + hdr + ";" + p + "\x1b\\")
    return "".join(out)


case("f_chunked", [root(), chunked("i=1,f=32,s=4,v=2", solid(G))])
case("f_chunked_bare", [root(), chunked("i=1,f=32,s=4,v=2", solid(G)).replace("a=f,m=", "m=")])
case("f_chunked_unknown", [chunked("i=77,f=32,s=4,v=2", solid(G))])
case("f_chunked_then_transmit", [root(), chunked("i=1,f=32,s=4,v=2", solid(G)),
                                 apc("a=t,f=32,s=1,v=1,i=2", px(R))])

# ---- a=a replies ----
case("a_replies", [root(), frame("i=1,q=2,f=32,s=4,v=2", solid(G)),
                   apc("a=a,i=1,s=3"), apc("a=a,i=1,s=1"), apc("a=a,i=1,c=9"),
                   apc("a=a,i=1,r=9,z=10"), apc("a=a,i=1,s=9"), apc("a=a,i=1,v=3"),
                   apc("a=a,i=1,c=2"), apc("a=a,i=99"), apc("a=a,I=99"), apc("a=a,s=3"),
                   apc("a=a,i=1,q=0,s=3,p=4")])

# ---- a=c replies ----
twof = [root(), frame("i=1,q=2,f=32,s=4,v=2", solid(G))]
case("c_defaults", twof + [apc("a=c,i=1")])
case("c_ok", twof + [apc("a=c,i=1,r=1,c=2"), apc("a=c,i=1,r=2,c=1,w=1,h=1"),
                     apc("a=c,i=1,r=2,c=1,q=1")])
case("c_missing_frames", twof + [apc("a=c,i=1,r=3,c=1"), apc("a=c,i=1,r=1,c=3"),
                                 apc("a=c,i=1,r=3,c=3"), apc("a=c,i=1,r=1"),
                                 apc("a=c,i=1,c=1")])
case("c_bounds", twof + [apc("a=c,i=1,r=1,c=2,w=5"), apc("a=c,i=1,r=1,c=2,x=1"),
                         apc("a=c,i=1,r=1,c=2,X=1"), apc("a=c,i=1,r=1,c=2,w=2,h=1,X=3"),
                         apc("a=c,i=1,r=1,c=2,w=2,h=1,x=3"), apc("a=c,i=1,r=1,c=2,h=3"),
                         apc("a=c,i=1,r=1,c=2,w=2,h=2,x=2,X=2")])
case("c_overlap", twof + [apc("a=c,i=1,r=2,c=2,w=1,h=1"), apc("a=c,i=1,r=2,c=2,w=1,h=1,x=1"),
                          apc("a=c,i=1,r=2,c=2,w=2,h=1,x=1"), apc("a=c,i=1,r=1,c=1,w=2,h=2,X=2")])
case("c_unknown", [apc("a=c,i=99,r=1,c=1"), apc("a=c,I=99"), apc("a=c,r=1,c=1"),
                   apc("a=c,i=99,q=1"), apc("a=c,i=99,q=2")])
case("c_number", [root(i=0, extra=",I=9"), frame("I=9,q=2,f=32,s=4,v=2", solid(G)),
                  apc("a=c,I=9,r=1,c=2"), apc("a=c,I=9,r=1,c=5")])
case("c_quiet", twof + [apc("a=c,i=1,r=1,c=2,q=1"), apc("a=c,i=1,r=1,c=5,q=1"),
                        apc("a=c,i=1,r=1,c=5,q=2")])

# ---- frame count probes after deletes (new-frame reply tells the count) ----
three = [root(), frame("i=1,q=2,f=32,s=4,v=2", solid(G)), frame("i=1,q=2,f=32,s=4,v=2", solid(B))]
probe = frame("i=1,f=32,s=4,v=2", solid(W))
case("d_f_default", three + [apc("a=d,d=f,i=1"), probe])
case("d_f_r2", three + [apc("a=d,d=f,i=1,r=2"), probe])
case("d_f_r9", three + [apc("a=d,d=f,i=1,r=9"), probe])
case("d_F_frames", three + [apc("a=d,d=F,i=1"), probe])
case("d_F_until_gone", three + [apc("a=d,d=F,i=1"), apc("a=d,d=F,i=1"), apc("a=d,d=F,i=1"), probe])
case("d_f_until_one", three + [apc("a=d,d=f,i=1"), apc("a=d,d=f,i=1"), apc("a=d,d=f,i=1"), probe])
case("d_f_number", [root(i=0, extra=",I=3"), frame("I=3,q=2,f=32,s=4,v=2", solid(G)),
                    apc("a=d,d=f,I=3"), frame("I=3,f=32,s=4,v=2", solid(G))])

# ---- pixel cases (screenshot) ----
# Which frame is current after the commands. Frames: 1 red, 2 green, 3 blue.
case("px_initial_after_frames", three + [place()], shot=True)
case("px_current_c2", three + [apc("a=a,i=1,c=2"), place()], shot=True)
case("px_current_c3_then_c9", three + [apc("a=a,i=1,c=3"), apc("a=a,i=1,c=9"), place()], shot=True)
case("px_d_f_default_cur", three + [apc("a=a,i=1,c=3"), apc("a=d,d=f,i=1"), place()], shot=True)
case("px_d_f_r2_cur3", three + [apc("a=a,i=1,c=3"), apc("a=d,d=f,i=1,r=2"), place()], shot=True)
case("px_d_f_r1_cur1", three + [apc("a=d,d=f,i=1,r=1"), place()], shot=True)
case("px_d_f_r3_cur3", three + [apc("a=a,i=1,c=3"), apc("a=d,d=f,i=1,r=3"), place()], shot=True)
# Partial frames: background, base frame, blend vs overwrite.
case("px_partial_default_bg", [root(col=W), frame("i=1,f=32,s=2,v=1,x=1,y=1", px(G, G)),
                               apc("a=a,i=1,c=2"), place()], shot=True)
case("px_partial_Y_bg", [root(col=W), frame("i=1,f=32,s=2,v=1,x=1,y=1,Y=4278190335", px(G, G)),
                         apc("a=a,i=1,c=2"), place()], shot=True)
case("px_partial_base1", [root(col=W), frame("i=1,f=32,s=2,v=1,x=1,y=1,c=1", px(G, G)),
                          apc("a=a,i=1,c=2"), place()], shot=True)
case("px_partial_base1_blend", [root(col=W), frame("i=1,f=32,s=2,v=1,x=1,y=1,c=1", px(HR, T)),
                                apc("a=a,i=1,c=2"), place()], shot=True)
case("px_partial_base1_X1", [root(col=W), frame("i=1,f=32,s=2,v=1,x=1,y=1,c=1,X=1", px(HR, T)),
                             apc("a=a,i=1,c=2"), place()], shot=True)
case("px_full_frame_Y_translucent", [root(col=W), frame("i=1,f=32,s=4,v=2,Y=4278190335", solid(HG)),
                                     apc("a=a,i=1,c=2"), place()], shot=True)
case("px_partial_Y_blend", [root(col=W), frame("i=1,f=32,s=2,v=1,Y=4278190335", px(HG, T)),
                            apc("a=a,i=1,c=2"), place()], shot=True)
case("px_edit_r2", [root(col=W), frame("i=1,f=32,s=4,v=2", solid(G)),
                    frame("i=1,r=2,f=32,s=1,v=1,x=0,y=0", px(B)),
                    frame("i=1,r=2,f=32,s=1,v=1,x=3,y=1,Y=4278190335", px(T)),
                    apc("a=a,i=1,c=2"), place()], shot=True)
case("px_edit_r1_root", [root(col=W), frame("i=1,r=1,f=32,s=1,v=1,x=0,y=0", px(B)), place()], shot=True)
case("px_edit_root_live_ref", [root(col=W), frame("i=1,f=32,s=1,v=1,x=0,y=0,c=1", px(G)),
                               frame("i=1,r=1,f=32,s=1,v=1,x=3,y=1", px(B)),
                               apc("a=a,i=1,c=2"), place()], shot=True)
case("px_offset_clip", [root(col=W), frame("i=1,f=32,s=2,v=2,x=3,y=1,c=1", px(B, B, B, B)),
                        apc("a=a,i=1,c=2"), place()], shot=True)
# Composition.
case("px_compose_blend", [root(col=W), frame("i=1,q=2,f=32,s=4,v=2", solid(HR)),
                          apc("a=c,i=1,r=2,c=1,w=2,h=1,x=2,y=1"), place()], shot=True)
case("px_compose_C1", [root(col=W), frame("i=1,q=2,f=32,s=4,v=2", solid(HR)),
                       apc("a=c,i=1,r=2,c=1,w=2,h=1,x=2,y=1,C=1"), place()], shot=True)
case("px_compose_src_offset", [root(col=W), frame("i=1,q=2,f=32,s=4,v=2", px(R, G, B, K, W, W, W, W)),
                               apc("a=c,i=1,r=2,c=1,w=2,h=1,X=1,Y=0,x=0,y=1,C=1"), place()], shot=True)
# Animation without a=a: does it run?
case("px_no_control_runs", [root(), frame("i=1,q=2,f=32,s=4,v=2,z=50", solid(G)),
                            frame("i=1,q=2,f=32,s=4,v=2,z=50", solid(B)), place()], shot=True, settle=0.6)
case("px_s3_runs_loops", [root(), frame("i=1,q=2,f=32,s=4,v=2,z=100", solid(G)),
                          frame("i=1,q=2,f=32,s=4,v=2,z=100", solid(B)),
                          apc("a=a,i=1,r=1,z=100"), apc("a=a,i=1,s=3,v=2"), place()], shot=True, settle=1.0)
case("px_s2_loading_stops", [root(), frame("i=1,q=2,f=32,s=4,v=2,z=60", solid(G)),
                             frame("i=1,q=2,f=32,s=4,v=2,z=60", solid(B)),
                             apc("a=a,i=1,r=1,z=60"), apc("a=a,i=1,s=2"), place()], shot=True, settle=1.0)
case("px_root_gapless_skip", [root(), frame("i=1,q=2,f=32,s=4,v=2,z=60", solid(G)),
                              frame("i=1,q=2,f=32,s=4,v=2,z=60", solid(B)),
                              apc("a=a,i=1,s=3,v=2"), place()], shot=True, settle=1.0)

case("px_nc2_nogap", [root(), frame("i=1,q=2,f=32,s=4,v=2", solid(G)),
                      frame("i=1,q=2,f=32,s=4,v=2", solid(B)), place()], shot=True)
case("px_nc3_c1", [root(), frame("i=1,q=2,f=32,s=4,v=2,z=50", solid(G)),
                   frame("i=1,q=2,f=32,s=4,v=2,z=50", solid(B)), apc("a=a,i=1,c=1"), place()], shot=True)
case("px_nc4_placed_first", [root(), place(), frame("i=1,q=2,f=32,s=4,v=2,z=50", solid(G)),
                   frame("i=1,q=2,f=32,s=4,v=2,z=50", solid(B))], shot=True)
case("px_nc5_one_frame", [root(), frame("i=1,q=2,f=32,s=4,v=2,z=50", solid(G)), place()], shot=True)

case("fc_chunked_count", [root(), chunked("i=1,q=2,f=32,s=4,v=2", solid(G)), frame("i=1,f=32,s=4,v=2", solid(W))])
case("fc_chunked_bare_count", [root(), chunked("i=1,q=2,f=32,s=4,v=2", solid(G)).replace("a=f,m=", "m="), frame("i=1,f=32,s=4,v=2", solid(W))])
case("fc_chunked_noq_count", [root(), chunked("i=1,f=32,s=4,v=2", solid(G)), frame("i=1,f=32,s=4,v=2", solid(W))])
case("fc_chunked_id_on_last", [root(), chunked("i=1,f=32,s=4,v=2", solid(G)).replace("a=f,m=0", "a=f,i=1,m=0")])
case("fc_chunk_err", [root(), chunked("i=1,c=7,f=32,s=4,v=2", solid(G)), frame("i=1,f=32,s=4,v=2", solid(W))])
case("fc_root_replace_resets", [root(), frame("i=1,q=2,f=32,s=4,v=2", solid(G)), root(), frame("i=1,f=32,s=4,v=2", solid(W))])
case("fc_t_d_on_frame_s_mismatch", [root(), frame("i=1,f=24,s=4,v=2", bytes(24))])
case("fc_frame_on_placeholder_only", [apc("a=T,U=1,f=32,s=4,v=2,i=5,q=2", solid(R)), frame("i=5,f=32,s=4,v=2", solid(G))])
case("fc_edit_gap", [root(), frame("i=1,q=2,f=32,s=4,v=2", solid(G)), frame("i=1,r=2,z=70,f=32,s=1,v=1", px(B))])

case("px_chain_lazy", [root(col=W), frame("i=1,q=2,f=32,s=1,v=1,c=1", px(G)),
                       frame("i=1,q=2,f=32,s=1,v=1,x=1,c=2", px(B)),
                       frame("i=1,q=2,r=1,f=32,s=1,v=1,x=3,y=1", px(R)),
                       apc("a=a,i=1,c=3"), place()], shot=True)
case("px_chain_big", [root(col=W), frame("i=1,q=2,f=32,s=4,v=2,c=1", solid(T)),
                      frame("i=1,q=2,f=32,s=1,v=1,x=1,c=2", px(B)),
                      frame("i=1,q=2,r=1,f=32,s=1,v=1,x=3,y=1", px(R)),
                      apc("a=a,i=1,c=3"), place()], shot=True)
case("px_chain_big_f2", [root(col=W), frame("i=1,q=2,f=32,s=4,v=2,c=1", solid(T)),
                      frame("i=1,q=2,f=32,s=1,v=1,x=1,c=2", px(B)),
                      frame("i=1,q=2,r=1,f=32,s=1,v=1,x=3,y=1", px(R)),
                      apc("a=a,i=1,c=2"), place()], shot=True)
case("px_del_base", [root(col=W), frame("i=1,q=2,f=32,s=1,v=1,c=1", px(G)),
                     apc("a=d,d=f,i=1,r=1"), place()], shot=True)
case("px_compose_live_ref", [root(col=W), frame("i=1,q=2,f=32,s=1,v=1,c=1", px(G)),
                     frame("i=1,q=2,f=32,s=4,v=2", solid(B)),
                     apc("a=c,i=1,r=3,c=1,w=1,h=1,x=3,y=1,C=1"),
                     apc("a=a,i=1,c=2"), place()], shot=True)
case("fe_order_bad_data_missing_base", [root(), frame("i=1,c=5,f=32,s=4,v=2", b"xx"),
                                        frame("i=1,c=5,f=32,s=5,v=2", solid(G,5,2)),
                                        frame("i=1,r=1,f=32,s=5,v=2", solid(G,5,2)),
                                        frame("i=1,r=1,c=9,f=32,s=4,v=2", solid(G))])

case("z_put_number_unknown", [apc("a=p,I=55"), apc("a=p,I=55,p=3"), apc("a=f,I=55,p=3")])
case("z_put_number_known", [root(i=0, extra=",I=56"), apc("a=p,I=56,q=0,C=1")])

def chunked_ids(keys, data, cont, n=8):
    enc = b64(data)
    parts = [enc[i:i + n] for i in range(0, len(enc), n)]
    out = []
    for k, p in enumerate(parts):
        more = 1 if k < len(parts) - 1 else 0
        hdr = (f"a=f,{keys},m={more}" if k == 0 else f"{cont},m={more}")
        out.append("\x1b_G" + hdr + ";" + p + "\x1b\\")
    return "".join(out)
case("fc_cont_with_i", [root(), frame("i=1,q=2,f=32,s=4,v=2", solid(G)), frame("i=1,q=2,f=32,s=4,v=2", solid(G)),
                        chunked_ids("i=1,f=32,s=4,v=2", solid(B), "a=f,i=1"), frame("i=1,f=32,s=4,v=2", solid(W))])
case("fc_cont_with_i_no_a", [root(), chunked_ids("i=1,f=32,s=4,v=2", solid(B), "i=1"), frame("i=1,f=32,s=4,v=2", solid(W))])
case("fc_cont_with_I", [root(i=0, extra=",I=4"), chunked_ids("I=4,f=32,s=4,v=2", solid(B), "a=f,I=4"), frame("I=4,f=32,s=4,v=2", solid(W))])
case("fc_cont_q1", [root(), chunked_ids("i=1,q=1,f=32,s=4,v=2", solid(B), "a=f,i=1"), frame("i=1,f=32,s=4,v=2", solid(W))])
case("fc_cont_edit", [root(), frame("i=1,q=2,f=32,s=4,v=2", solid(G)), chunked_ids("i=1,r=2,f=32,s=4,v=2", solid(B), "a=f,i=1"), frame("i=1,f=32,s=4,v=2", solid(W))])


def out(s):
    sys.stdout.write(s)
    sys.stdout.flush()


def read_until_da(fd, timeout=3.0):
    buf = b""
    end = time.time() + timeout
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            buf += os.read(fd, 65536)
            # DA1 reply: ESC [ ? ... c
            i = buf.find(b"\x1b[?")
            if i >= 0 and buf.find(b"c", i) >= 0:
                j = buf.find(b"c", i)
                return buf[:i] + buf[j + 1:], True
    return buf, False


def split_replies(b):
    out = []
    while b:
        i = b.find(b"\x1b_G")
        if i < 0:
            if b.strip():
                out.append("JUNK:" + repr(b))
            break
        j = b.find(b"\x1b\\", i)
        if j < 0:
            out.append("PARTIAL:" + repr(b[i:]))
            break
        out.append(b[i + 3:j].decode("utf-8", "replace"))
        b = b[j + 2:]
    return out


def main():
    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    tty.setraw(fd)
    results = []
    try:
        for c in CASES:
            if ONLY and not any(c["name"].startswith(p) for p in ONLY):
                continue
            out("\x1b[2J\x1b[H" + apc("a=d,d=A,q=2"))
            time.sleep(0.05)
            out("".join(c["cmds"]))
            out("\x1b[c")
            replies, ok = read_until_da(fd)
            rec = {"name": c["name"], "replies": split_replies(replies), "sentinel": ok}
            if c["shot"]:
                time.sleep(float(os.environ.get("SETTLE","0.25")) + c["settle"])
                req = os.path.join(SHOTDIR, "req")
                ack = os.path.join(SHOTDIR, "ack")
                with open(req + ".tmp", "w") as f:
                    f.write(c["name"])
                os.rename(req + ".tmp", req)
                end = time.time() + 10
                while not os.path.exists(ack) and time.time() < end:
                    time.sleep(0.02)
                if os.path.exists(ack):
                    os.unlink(ack)
                rec["shot"] = c["name"]
            results.append(rec)
        out("\x1b[2J\x1b[H" + apc("a=d,d=A,q=2"))
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)
    with open(OUT, "w") as f:
        for r in results:
            f.write(json.dumps(r) + "\n")
    print("probe done", len(results))


main()
