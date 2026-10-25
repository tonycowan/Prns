/* A small picture encoder and decoder for the journal call.
 *
 * libvpx is linked in. The buffers this file returns are owned by the caller
 * and freed with optio_vpx_free. One context is used from one thread at a time.
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vpx/vp8cx.h>
#include <vpx/vp8dx.h>
#include <vpx/vpx_decoder.h>
#include <vpx/vpx_encoder.h>

struct OptioVpx {
  vpx_codec_ctx_t ctx;
  vpx_image_t img;
  int encode;
  unsigned int w;
  unsigned int h;
  char error[160];
};

static void copy_in(unsigned char *dst, int stride, const uint8_t *src, int w, int h) {
  int y;
  for (y = 0; y < h; y++) {
    memcpy(dst + (size_t)y * (size_t)stride, src + (size_t)y * (size_t)w, (size_t)w);
  }
}

static void copy_out(uint8_t *dst, const unsigned char *src, int stride, int w, int h) {
  int y;
  for (y = 0; y < h; y++) {
    memcpy(dst + (size_t)y * (size_t)w, src + (size_t)y * (size_t)stride, (size_t)w);
  }
}

struct OptioVpx *optio_vpx_encoder(int w, int h, int kbps, int fps) {
  struct OptioVpx *enc;
  vpx_codec_enc_cfg_t cfg;
  if (w < 16 || h < 16 || (w % 2) != 0 || (h % 2) != 0 || kbps < 1 || fps < 1) {
    return NULL;
  }
  enc = calloc(1, sizeof(*enc));
  if (enc == NULL) {
    return NULL;
  }
  enc->encode = 1;
  enc->w = (unsigned int)w;
  enc->h = (unsigned int)h;
  if (vpx_codec_enc_config_default(vpx_codec_vp8_cx(), &cfg, 0) != VPX_CODEC_OK) {
    free(enc);
    return NULL;
  }
  cfg.g_w = enc->w;
  cfg.g_h = enc->h;
  cfg.g_threads = 1;
  cfg.g_timebase.num = 1;
  cfg.g_timebase.den = fps;
  cfg.g_lag_in_frames = 0;
  cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;
  cfg.rc_end_usage = VPX_CBR;
  cfg.rc_target_bitrate = (unsigned int)kbps;
  cfg.kf_mode = VPX_KF_AUTO;
  cfg.kf_min_dist = (unsigned int)fps;
  cfg.kf_max_dist = (unsigned int)fps;
  if (vpx_codec_enc_init(&enc->ctx, vpx_codec_vp8_cx(), &cfg, 0) != VPX_CODEC_OK) {
    fprintf(stderr, "picture encoder: %s\n", vpx_codec_error(&enc->ctx));
    free(enc);
    return NULL;
  }
  /* A higher value spends less time per frame, which is what a live call can spare. */
  vpx_codec_control(&enc->ctx, VP8E_SET_CPUUSED, 8);
  if (vpx_img_alloc(&enc->img, VPX_IMG_FMT_I420, enc->w, enc->h, 32) == NULL) {
    vpx_codec_destroy(&enc->ctx);
    free(enc);
    return NULL;
  }
  return enc;
}

uint8_t *optio_vpx_encode(struct OptioVpx *enc, const uint8_t *i420, int64_t pts, int key,
                         int *out_len) {
  const uint8_t *u;
  const uint8_t *v;
  vpx_codec_iter_t iter;
  const vpx_codec_cx_pkt_t *pkt;
  size_t total;
  uint8_t *out;
  size_t filled;
  size_t y_size;
  size_t c_size;
  vpx_enc_frame_flags_t flags;
  if (out_len != NULL) {
    *out_len = 0;
  }
  if (enc == NULL || !enc->encode || i420 == NULL || out_len == NULL) {
    return NULL;
  }
  y_size = (size_t)enc->w * (size_t)enc->h;
  c_size = y_size / 4;
  u = i420 + y_size;
  v = u + c_size;
  copy_in(enc->img.planes[VPX_PLANE_Y], enc->img.stride[VPX_PLANE_Y], i420, (int)enc->w,
          (int)enc->h);
  copy_in(enc->img.planes[VPX_PLANE_U], enc->img.stride[VPX_PLANE_U], u, (int)enc->w / 2,
          (int)enc->h / 2);
  copy_in(enc->img.planes[VPX_PLANE_V], enc->img.stride[VPX_PLANE_V], v, (int)enc->w / 2,
          (int)enc->h / 2);
  flags = key ? VPX_EFLAG_FORCE_KF : 0;
  if (vpx_codec_encode(&enc->ctx, &enc->img, pts, 1, flags, VPX_DL_REALTIME) != VPX_CODEC_OK) {
    return NULL;
  }
  total = 0;
  iter = NULL;
  while ((pkt = vpx_codec_get_cx_data(&enc->ctx, &iter)) != NULL) {
    if (pkt->kind == VPX_CODEC_CX_FRAME_PKT) {
      total += pkt->data.frame.sz;
    }
  }
  if (total == 0 || total > 65536) {
    return NULL;
  }
  out = malloc(total);
  if (out == NULL) {
    return NULL;
  }
  filled = 0;
  iter = NULL;
  while ((pkt = vpx_codec_get_cx_data(&enc->ctx, &iter)) != NULL) {
    if (pkt->kind == VPX_CODEC_CX_FRAME_PKT) {
      memcpy(out + filled, pkt->data.frame.buf, pkt->data.frame.sz);
      filled += pkt->data.frame.sz;
    }
  }
  *out_len = (int)filled;
  return out;
}

void optio_vpx_encoder_free(struct OptioVpx *enc) {
  if (enc == NULL) {
    return;
  }
  vpx_img_free(&enc->img);
  vpx_codec_destroy(&enc->ctx);
  free(enc);
}

struct OptioVpx *optio_vpx_decoder(void) {
  struct OptioVpx *dec;
  vpx_codec_dec_cfg_t cfg;
  dec = calloc(1, sizeof(*dec));
  if (dec == NULL) {
    return NULL;
  }
  memset(&cfg, 0, sizeof(cfg));
  cfg.threads = 1;
  if (vpx_codec_dec_init(&dec->ctx, vpx_codec_vp8_dx(), &cfg, 0) != VPX_CODEC_OK) {
    fprintf(stderr, "picture decoder: %s\n", vpx_codec_error(&dec->ctx));
    free(dec);
    return NULL;
  }
  return dec;
}

uint8_t *optio_vpx_decode(struct OptioVpx *dec, const uint8_t *buf, int len, int *w, int *h) {
  vpx_codec_iter_t iter;
  vpx_image_t *img;
  size_t y_size;
  size_t c_size;
  uint8_t *out;
  if (w != NULL) {
    *w = 0;
  }
  if (h != NULL) {
    *h = 0;
  }
  if (dec == NULL || dec->encode || buf == NULL || len < 1 || w == NULL || h == NULL) {
    return NULL;
  }
  dec->error[0] = '\0';
  if (vpx_codec_decode(&dec->ctx, buf, (unsigned int)len, NULL, 0) != VPX_CODEC_OK) {
    const char *detail = vpx_codec_error_detail(&dec->ctx);
    if (detail != NULL && detail[0] != '\0') {
      snprintf(dec->error, sizeof(dec->error), "%s: %s", vpx_codec_error(&dec->ctx), detail);
    } else {
      snprintf(dec->error, sizeof(dec->error), "%s", vpx_codec_error(&dec->ctx));
    }
    return NULL;
  }
  iter = NULL;
  img = vpx_codec_get_frame(&dec->ctx, &iter);
  if (img == NULL) {
    snprintf(dec->error, sizeof(dec->error), "%s", "no picture");
    return NULL;
  }
  if (img->d_w == 0 || img->d_h == 0 || (img->d_w % 2) != 0 || (img->d_h % 2) != 0 ||
      img->d_w > 640 || img->d_h > 480) {
    snprintf(dec->error, sizeof(dec->error), "picture %u x %u", img->d_w, img->d_h);
    return NULL;
  }
  y_size = (size_t)img->d_w * (size_t)img->d_h;
  c_size = y_size / 4;
  out = malloc(y_size + c_size + c_size);
  if (out == NULL) {
    return NULL;
  }
  copy_out(out, img->planes[VPX_PLANE_Y], img->stride[VPX_PLANE_Y], (int)img->d_w, (int)img->d_h);
  copy_out(out + y_size, img->planes[VPX_PLANE_U], img->stride[VPX_PLANE_U], (int)img->d_w / 2,
           (int)img->d_h / 2);
  copy_out(out + y_size + c_size, img->planes[VPX_PLANE_V], img->stride[VPX_PLANE_V],
           (int)img->d_w / 2, (int)img->d_h / 2);
  *w = (int)img->d_w;
  *h = (int)img->d_h;
  return out;
}

const char *optio_vpx_error(struct OptioVpx *dec) {
  if (dec == NULL) {
    return "";
  }
  return dec->error;
}

void optio_vpx_decoder_free(struct OptioVpx *dec) {
  if (dec == NULL) {
    return;
  }
  vpx_codec_destroy(&dec->ctx);
  free(dec);
}

void optio_vpx_free(uint8_t *bytes) { free(bytes); }
